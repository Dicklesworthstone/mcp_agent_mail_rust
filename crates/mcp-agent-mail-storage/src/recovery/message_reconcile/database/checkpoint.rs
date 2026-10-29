//! Restart-safe scheduling hints for the live archive reconciler.
//!
//! Scan positions and retry debt are published together. A crash before that
//! publication replays the previous bounded pass instead of forgetting a
//! deferred ID behind a newer high-water mark. Nothing in this file is archive
//! authority: every restored ID still traverses the normal live-source checks.

use std::collections::{HashMap, HashSet, VecDeque};
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

// Version 1 bound hints only to the inode. In-place restore/reconstruction can
// change the mailbox generation without replacing that file. Such hints must
// be ignored rather than attached to the restored mailbox's reused row IDs.
const SCHEMA_VERSION: u32 = 2;
const MAX_CHECKPOINT_BYTES: u64 = 256 * 1024;
const CHECKPOINT_OWNED: &str =
    "message reconciliation deferred: checkpoint is owned by another worker";

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    database: PathBuf,
    archive: PathBuf,
    // The file identity, not its length/mtime: ordinary commits must not
    // invalidate a checkpoint. Replacement databases must start a fresh scan.
    generation: String,
    database_generation: String,
}

impl Binding {
    fn capture(cx: &Cx, pool: &DbPool, config: &Config) -> Result<Self, String> {
        let generation = source_generation(cx, pool)?;
        Self::at(
            Path::new(pool.sqlite_path()),
            &config.storage_root,
            &generation,
        )
        .map_err(|error| error.to_string())
    }

    fn at(database: &Path, archive: &Path, database_generation: &str) -> io::Result<Self> {
        if database_generation.is_empty()
            || database_generation.len() > 128
            || !database_generation
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("checkpoint database generation is invalid"));
        }
        let database = std::fs::canonicalize(database)
            .map_err(|error| io_context("canonicalize database", database, error))?;
        let archive = std::fs::canonicalize(archive)
            .map_err(|error| io_context("canonicalize archive", archive, error))?;
        let metadata = std::fs::metadata(&database)
            .map_err(|error| io_context("read database metadata", &database, error))?;
        let archive_metadata = std::fs::metadata(&archive)
            .map_err(|error| io_context("read archive metadata", &archive, error))?;
        if !metadata.is_file() || !archive_metadata.is_dir() {
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
            database_generation: database_generation.to_string(),
        })
    }

    fn path(&self) -> io::Result<PathBuf> {
        checkpoint_path(&self.database, &self.archive)
    }
}

/// The namespace depends only on the canonical paths, not a logical generation.
/// Computing it is not source authorization and must not query the database.
fn checkpoint_path(database: &Path, archive: &Path) -> io::Result<PathBuf> {
    let parent = database
        .parent()
        .ok_or_else(|| invalid("checkpoint source has no parent directory"))?;
    let mut digest = Sha256::new();
    digest.update(database.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(archive.as_os_str().as_encoded_bytes());
    // Keep maintenance hints beside SQLite, not inside an archive Git tree.
    // Different archive roots for the same database cannot share progress.
    Ok(parent.join(format!(
        ".am-message-reconcile-{}.json",
        hex::encode(digest.finalize())
    )))
}

/// Best-effort negative admission only. Never create/truncate a sidecar here.
/// A successful probe releases its descriptor before source work: the original
/// Store::open still acquires and validates the authoritative lock afterwards.
fn probe_existing_checkpoint_lock(database: &Path, archive: &Path) -> io::Result<()> {
    let database = std::fs::canonicalize(database)?;
    let archive = std::fs::canonicalize(archive)?;
    let path = checkpoint_path(&database, &archive)?;
    check_sidecar(&path)?;
    let lock_path = path.with_extension("json.lock");
    check_sidecar(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(false).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let lock = options
        .open(&lock_path)
        .map_err(|error| io_context("open existing lock", &lock_path, error))?;
    if !lock
        .metadata()
        .map_err(|error| io_context("read opened lock metadata", &lock_path, error))?
        .is_file()
    {
        return Err(invalid("checkpoint lock is not a regular file"));
    }
    try_lock_checkpoint(&lock, &lock_path)
}

/// GH#354: refuse known checkpoint contention BEFORE pool acquisition and the
/// query-only/generation probes. Missing/unsafe/unavailable metadata is not a
/// positive verdict; the existing source authorization and Store checks still
/// run and own all errors/fallback decisions. A racing new owner is checked
/// again by Store::open; this hint never grants publication authority.
fn with_available_checkpoint<T>(
    database: &Path,
    archive: &Path,
    source: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if probe_existing_checkpoint_lock(database, archive)
        .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
    {
        return Err(CHECKPOINT_OWNED.into());
    }
    source()
}

/// The caller holds nonblocking database-promotion admission. Query-only
/// sources must refuse before either identity initialization or checkpoint I/O.
/// Fresh mailboxes have an empty identity table until their first reservation;
/// archive recovery cannot depend on a reservation ever being created. Seed an
/// empty table once on this authorized writable connection, without replacing
/// an existing identity, then release the connection before filesystem I/O.
fn source_generation(cx: &Cx, pool: &DbPool) -> Result<String, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
        return Err("query-only snapshots cannot authorize message archive repair".into());
    }
    // Read the whole singleton relation (bounded), not just row zero: an
    // unexpected row must not be mistaken for a fresh, uninitialized mailbox.
    let select = "SELECT singleton, generation_id FROM db_identity LIMIT 2";
    let mut rows = conn.query_sync(select, &[]).map_err(source_error)?;
    if rows.is_empty() {
        if cx.checkpoint().is_err() || corruption_circuit_breaker().is_tripped() {
            return Err("message checkpoint identity initialization interrupted".into());
        }
        if mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level
            .load()
            >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
        {
            return Err(
                "message checkpoint identity initialization deferred by critical disk pressure"
                    .into(),
            );
        }
        let token = mcp_agent_mail_core::setup::generate_token().map_err(|error| {
            format!("message checkpoint identity initialization failed: {error}")
        })?;
        // Match the reservation writer's lazy generation initialization. The
        // unique singleton key elects one durable token across racing workers;
        // always read back the winner rather than trusting our proposed value.
        conn.execute_sync(
            "INSERT OR IGNORE INTO db_identity (singleton, generation_id) VALUES (0, ?)",
            &[mcp_agent_mail_db::sqlmodel_core::Value::Text(token)],
        )
        .map_err(source_error)?;
        rows = conn.query_sync(select, &[]).map_err(source_error)?;
    }
    let [row] = rows.as_slice() else {
        return Err("message checkpoint requires one database generation".into());
    };
    if row.get_named::<i64>("singleton").map_err(source_error)? != 0 {
        return Err("message checkpoint database generation has an invalid singleton".into());
    }
    row.get_named::<String>("generation_id")
        .map_err(source_error)
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

#[derive(Debug)]
struct CheckpointIoError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for CheckpointIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "checkpoint {} ({}): {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for CheckpointIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn io_context(operation: &'static str, path: &Path, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        CheckpointIoError {
            operation,
            path: path.to_path_buf(),
            source,
        },
    )
}

fn try_lock_checkpoint(lock: &File, path: &Path) -> io::Result<()> {
    FileExt::try_lock_exclusive(lock).map_err(|source| {
        // fs2 returns ERROR_LOCK_VIOLATION on Windows, which std does not map
        // to WouldBlock. A live owner must defer the pass, never select the
        // unavailable-checkpoint fallback that runs without durable progress.
        let contended = source
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error());
        let kind = if contended {
            io::ErrorKind::WouldBlock
        } else {
            source.kind()
        };
        io::Error::new(
            kind,
            CheckpointIoError {
                operation: "acquire exclusive lock",
                path: path.to_path_buf(),
                source,
            },
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreState {
    Absent,
    Restored,
    Merged,
    CapacityBlocked,
}

/// Merge obligations, not authority or old scan positions. The per-pass flock
/// permits another cursor to publish while this cursor has an unsaved pass.
/// Ignoring that newer checkpoint would overwrite its deferred IDs on save;
/// replacing this cursor would instead discard its memory-only obligations.
/// Build both bounded queues before changing either, so saturation preserves
/// the in-memory cursor AND the on-disk checkpoint for a later pass.
fn merge_unsaved_debt(
    cursor: &mut ReconcileCursor,
    snapshot: Snapshot,
    now: Instant,
    wall_us: i64,
) -> bool {
    let clock_reversed = wall_us < snapshot.saved_at_us;
    let mut durable = ReconcileCursor::default();
    snapshot.restore(&mut durable, now, wall_us);
    if clock_reversed {
        // Reimporting the same future-dated snapshot must not restart its
        // relative delay on every failed save. Keep this cursor's monotonic
        // backoff below; newly discovered disk IDs can be inspected now.
        for retry in &mut durable.retries {
            retry.ready_at = now;
        }
        durable.conflict_backoff.clear();
    }
    let mut retries: VecDeque<DeferredRetry> = VecDeque::new();
    let mut retry_indices = HashMap::new();
    for retry in cursor.retries.iter().chain(&durable.retries) {
        if let Some(&index) = retry_indices.get(&retry.id) {
            let retained: &mut DeferredRetry = &mut retries[index];
            // An equal/older observation must not renew a local monotonic
            // deadline. A higher failure count is incorporated only once.
            if retry.attempts > retained.attempts {
                retained.attempts = retry.attempts;
                retained.ready_at = retained.ready_at.max(retry.ready_at);
            }
        } else {
            if retries.len() == MAX_RETRY_IDS {
                return false;
            }
            retry_indices.insert(retry.id, retries.len());
            retries.push_back(DeferredRetry {
                id: retry.id,
                attempts: retry.attempts,
                ready_at: retry.ready_at,
            });
        }
    }
    let mut conflicts: VecDeque<(i64, Instant)> = VecDeque::new();
    let mut conflict_indices = HashMap::new();
    for (local, id, until) in cursor
        .conflict_backoff
        .iter()
        .map(|&(id, until)| (true, id, until))
        .chain(
            durable
                .conflict_backoff
                .iter()
                .map(|&(id, until)| (false, id, until)),
        )
    {
        // A conflicting observation must not discard a separately retained
        // retry obligation. The next actual source/publication check decides
        // whether it is still transient. Expired cooldowns are not work debt.
        if until <= now {
            continue;
        }
        if let Some(&index) = retry_indices.get(&id) {
            if local {
                retries[index].ready_at = retries[index].ready_at.max(until);
            }
            continue;
        }
        if let Some(&index) = conflict_indices.get(&id) {
            if local {
                let retained: &mut (i64, Instant) = &mut conflicts[index];
                retained.1 = retained.1.max(until);
            }
        } else {
            if conflicts.len() == MAX_RETRY_IDS {
                return false;
            }
            conflict_indices.insert(id, conflicts.len());
            conflicts.push_back((id, until));
        }
    }
    cursor.retries = retries;
    cursor.conflict_backoff = conflicts;
    // Keep the live scan position and lane turns: reverting them on every
    // failed save can pin memory-only repair to its first healthy page. The
    // ordinary retry lane fairly rotates the union and rereads every source.
    true
}

fn check_sidecar(path: &Path) -> io::Result<()> {
    if crate::path_existing_prefix_has_symlink(path)
        .map_err(|error| io_context("validate path components", path, error))?
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
        Err(error) => Err(io_context("read sidecar metadata", path, error)),
    }
}

struct Store {
    binding: Binding,
    path: PathBuf,
    lock_path: PathBuf,
    // A nonblocking, per-source lock serializes checkpoint read/run/publish.
    // Keep the descriptor until the pass finishes; never wait on another server.
    lock: File,
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
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let lock = options
            .open(&lock_path)
            .map_err(|error| io_context("open or create lock", &lock_path, error))?;
        try_lock_checkpoint(&lock, &lock_path)?;
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
        check_sidecar(&self.lock_path)?;
        let opened = self
            .lock
            .metadata()
            .map_err(|error| io_context("read opened lock metadata", &self.lock_path, error))?;
        let named = std::fs::symlink_metadata(&self.lock_path)
            .map_err(|error| io_context("read named lock metadata", &self.lock_path, error))?;
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
        if opened
            .created()
            .map_err(|error| io_context("read opened lock creation time", &self.lock_path, error))?
            != named.created().map_err(|error| {
                io_context("read named lock creation time", &self.lock_path, error)
            })?
        {
            return Err(invalid("checkpoint lock identity changed"));
        }
        Ok(())
    }

    fn read_snapshot(&self) -> io::Result<Option<Snapshot>> {
        self.check_lock()?;
        check_sidecar(&self.path)?;
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &self.path,
            MAX_CHECKPOINT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_context("read snapshot", &self.path, error)),
        };
        let snapshot: Snapshot = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(&format!("checkpoint cannot be decoded: {error}")))?;
        self.check_lock()?;
        check_sidecar(&self.path)?;
        snapshot.validate(&snapshot.binding)?;
        Ok(Some(snapshot))
    }

    #[cfg(test)]
    fn load(&self) -> io::Result<Option<Snapshot>> {
        let Some(snapshot) = self.read_snapshot()? else {
            return Ok(None);
        };
        snapshot.validate(&self.binding)?;
        Ok(Some(snapshot))
    }

    fn restore(&self, cursor: &mut ReconcileCursor) -> io::Result<RestoreState> {
        let Some(snapshot) = self.read_snapshot()? else {
            return Ok(RestoreState::Absent);
        };
        if snapshot.binding != self.binding {
            // A generation can change BETWEEN passes, while the same pool and
            // cursor survive. Reject its disk watermarks and reset the live
            // scan too. Preserve all memory-only retry IDs on a failed save;
            // every ID still requires an authoritative current-source read.
            cursor.tail_after = None;
            cursor.backfill_ceiling = None;
            cursor.next_lane_is_history = false;
            cursor.startup_backfill_complete = false;
            cursor.startup_tail = None;
            cursor.startup_tail_initialized = false;
            return Err(invalid(
                "checkpoint source/archive generation does not match",
            ));
        }
        if cursor.checkpoint_dirty {
            return Ok(
                if merge_unsaved_debt(
                    cursor,
                    snapshot,
                    Instant::now(),
                    mcp_agent_mail_db::now_micros(),
                ) {
                    RestoreState::Merged
                } else {
                    RestoreState::CapacityBlocked
                },
            );
        }
        snapshot.restore(cursor, Instant::now(), mcp_agent_mail_db::now_micros());
        Ok(RestoreState::Restored)
    }

    fn save(&self, cursor: &ReconcileCursor) -> io::Result<()> {
        self.save_with(cursor, || Ok(()))
    }

    // The callback is a private deterministic test seam between real staging
    // and publication. Production passes a no-op, never an environment hook.
    fn save_with(
        &self,
        cursor: &ReconcileCursor,
        before_publish: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.check_lock()?;
        // Promotion is not held through filesystem I/O. Refuse to attach old
        // progress to a replacement database that appeared during the pass.
        let current = Binding::at(
            &self.binding.database,
            &self.binding.archive,
            &self.binding.database_generation,
        )?;
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
            .tempfile_in(parent)
            .map_err(|error| io_context("create staged snapshot", parent, error))?;
        candidate
            .write_all(&bytes)
            .map_err(|error| io_context("write staged snapshot", candidate.path(), error))?;
        candidate
            .as_file()
            .sync_all()
            .map_err(|error| io_context("sync staged snapshot", candidate.path(), error))?;
        before_publish()?;
        // A flock on an old inode grants no ownership of a replacement path.
        // Recheck after slow staging I/O, before replacing the paired hints.
        self.check_lock()?;
        check_sidecar(&self.path)?;
        candidate
            .persist(&self.path)
            .map_err(|error| io_context("publish staged snapshot", &self.path, error.error))?;
        #[cfg(unix)]
        File::open(parent)
            .map_err(|error| io_context("open snapshot directory", parent, error))?
            .sync_all()
            .map_err(|error| io_context("sync snapshot directory", parent, error))?;
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

/// Recheck the live logical generation before publishing progress. Do not
/// retain promotion admission across checkpoint fsync: a later replacement is
/// safe because this snapshot still names the OLD generation, not the new one.
fn save_current(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
    store: &Store,
) -> Result<(), String> {
    if stop.load(Ordering::Acquire)
        || cx.checkpoint().is_err()
        || corruption_circuit_breaker().is_tripped()
    {
        return Err("message checkpoint publication interrupted".into());
    }
    {
        let _admission = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
            .ok_or("message checkpoint publication deferred by recovery admission")?;
        validate_pool_binding(pool, config)?;
        if pool.sqlite_identity_key() != cursor.source_identity
            || Binding::capture(cx, pool, config)? != store.binding
        {
            // The next pass must not reuse this generation's in-memory
            // watermarks either. Its ordinary source reset clears old hints;
            // no current-generation row or archive artifact is changed here.
            cursor.source_identity.clear();
            return Err("checkpoint source changed during reconciliation".into());
        }
    }
    store.save(cursor).map_err(|error| error.to_string())
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
        let generation =
            with_available_checkpoint(Path::new(pool.sqlite_path()), &config.storage_root, || {
                source_generation(cx, pool)
            })?;
        scope = pool.sqlite_identity_key();
        if cursor.source_identity != scope {
            *cursor = ReconcileCursor {
                source_identity: scope.clone(),
                settled_before_us: cursor.settled_before_us,
                persist_progress: cursor.persist_progress,
                ..Default::default()
            };
        }
        match Binding::at(
            Path::new(pool.sqlite_path()),
            &config.storage_root,
            &generation,
        )
        .and_then(Store::open)
        {
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
    let mut save_permitted = true;
    if let Some(store) = &store {
        match store.restore(cursor) {
            Ok(RestoreState::Restored | RestoreState::Merged) => restored = true,
            Ok(RestoreState::Absent) => {}
            Ok(RestoreState::CapacityBlocked) => {
                // Still run the original bounded repair pass: it may free
                // enough capacity to merge next time. Neither cursor may
                // overwrite obligations that the union could not retain.
                save_permitted = false;
                warn_unavailable(&invalid("combined checkpoint retry debt exceeds its bound"));
            }
            Err(error) => warn_unavailable(&error),
        }
    }
    let mut result = reconcile_message_batch_inner(cx, pool, config, cursor, stop);
    cursor.checkpoint_dirty = true;
    let saved = if save_permitted && let Some(store) = &store {
        if pool.sqlite_identity_key() == scope && cursor.source_identity == scope {
            match save_current(cx, pool, config, cursor, stop, store) {
                Ok(()) => {
                    cursor.checkpoint_dirty = false;
                    true
                }
                Err(error) => {
                    warn_unavailable(&invalid(&error));
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

    const GENERATION: &str = "aabbccdd";

    #[test]
    fn unsaved_debt_merges_without_rewinding_the_live_scan_or_shortening_cooldowns() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        let mut durable = debt(now);
        durable.defer_transient(77, now);
        durable
            .conflict_backoff
            .push_back((88, now + Duration::from_secs(20)));
        let snapshot = Snapshot::capture(
            Binding::at(&database, &archive, GENERATION).unwrap(),
            &durable,
            now,
            1_000_000,
        );
        let mut cursor = ReconcileCursor::settled_before(999);
        cursor.source_identity = "current live pool".into();
        cursor.tail_after = Some(100_000);
        cursor.backfill_ceiling = Some(50);
        cursor.next_lane_is_history = true;
        cursor.next_lane_is_retry = true;
        cursor.startup_backfill_complete = true;
        cursor.checkpoint_dirty = true;
        cursor.defer_transient(80_000, now);
        cursor.defer_transient(88, now);
        cursor
            .conflict_backoff
            .push_back((77, now + Duration::from_secs(40)));
        cursor.conflict_backoff.push_back((91, now));
        assert!(merge_unsaved_debt(&mut cursor, snapshot, now, 11_000_000));
        assert_eq!(
            cursor
                .retries
                .iter()
                .map(|retry| retry.id)
                .collect::<Vec<_>>(),
            [80_000, 88, 7, 77],
        );
        assert_eq!(cursor.retries[0].attempts, 6);
        assert_eq!(cursor.retries[0].ready_at, now + Duration::from_secs(20));
        assert_eq!(cursor.retries[1].ready_at, now);
        assert_eq!(cursor.retries[3].ready_at, now + Duration::from_secs(40));
        assert_eq!(
            cursor.conflict_backoff.iter().copied().collect::<Vec<_>>(),
            [(19, now + Duration::from_secs(50))]
        );
        assert_eq!(
            (cursor.tail_after, cursor.backfill_ceiling),
            (Some(100_000), Some(50))
        );
        assert!(cursor.next_lane_is_history && cursor.next_lane_is_retry);
        assert!(cursor.startup_backfill_complete && cursor.checkpoint_dirty);
        assert_eq!(cursor.source_identity, "current live pool");
        assert_eq!(cursor.settled_before_us, 999);
    }

    #[test]
    fn dirty_cursor_retains_both_owners_after_a_real_failed_checkpoint_publication() {
        let (_temp, database, archive) = fixture();
        let binding = Binding::at(&database, &archive, GENERATION).unwrap();
        let first = Store::open(binding).unwrap();
        let mut cursor = debt(Instant::now());
        first.save(&cursor).unwrap();
        let original = std::fs::read(&first.path).unwrap();
        cursor.defer_transient(77, Instant::now());
        assert!(
            first
                .save_with(&cursor, || Err(io::Error::other(
                    "checkpoint publication failed"
                )))
                .is_err()
        );
        assert_eq!(std::fs::read(&first.path).unwrap(), original);
        cursor.checkpoint_dirty = true;
        drop(first);

        let second = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let mut other = ReconcileCursor::default();
        other.defer_transient(88, Instant::now());
        second.save(&other).unwrap();
        let newer = std::fs::read(&second.path).unwrap();
        drop(second);

        let resumed = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        assert_eq!(resumed.restore(&mut cursor).unwrap(), RestoreState::Merged);
        assert_eq!(
            std::fs::read(&resumed.path).unwrap(),
            newer,
            "merge is memory-only until publication"
        );
        resumed.save(&cursor).unwrap();
        let saved = resumed.load().unwrap().unwrap();
        assert_eq!(
            saved
                .retries
                .iter()
                .map(|retry| retry.id)
                .collect::<Vec<_>>(),
            [7, 80_000, 77, 88]
        );
        assert_eq!(saved.tail_after, Some(90_000));
    }

    #[test]
    fn saturated_union_changes_neither_queue_and_retries_after_capacity_is_freed() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let now = Instant::now();
        let mut other = ReconcileCursor::default();
        other.defer_transient(90_000, now);
        store.save(&other).unwrap();
        let original = std::fs::read(&store.path).unwrap();
        let mut cursor = ReconcileCursor {
            checkpoint_dirty: true,
            ..ReconcileCursor::default()
        };
        for id in 1..=i64::try_from(MAX_RETRY_IDS).unwrap() {
            cursor.defer_transient(id, now);
        }
        let before = format!("{cursor:?}");
        assert_eq!(
            store.restore(&mut cursor).unwrap(),
            RestoreState::CapacityBlocked
        );
        assert_eq!(format!("{cursor:?}"), before);
        assert_eq!(std::fs::read(&store.path).unwrap(), original);
        cursor.forget_retry(1);
        assert_eq!(store.restore(&mut cursor).unwrap(), RestoreState::Merged);
        assert_eq!(cursor.retries.len(), MAX_RETRY_IDS);
        assert_eq!(cursor.retries.front().unwrap().id, 2);
        assert_eq!(cursor.retries.back().unwrap().id, 90_000);
        store.save(&cursor).unwrap();
        assert_eq!(store.load().unwrap().unwrap().retries.len(), MAX_RETRY_IDS);
    }

    #[test]
    fn conflict_capacity_is_atomic_and_expiration_releases_the_union() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        let mut other = ReconcileCursor::default();
        other.defer_transient(90_000, now);
        other
            .conflict_backoff
            .push_back((80_000, now + MAX_RETRY_BACKOFF));
        let mut cursor = ReconcileCursor {
            checkpoint_dirty: true,
            ..ReconcileCursor::default()
        };
        for id in 1..=i64::try_from(MAX_RETRY_IDS).unwrap() {
            cursor
                .conflict_backoff
                .push_back((id, now + Duration::from_secs(1)));
        }
        let make_snapshot = || {
            Snapshot::capture(
                Binding::at(&database, &archive, GENERATION).unwrap(),
                &other,
                now,
                1_000_000,
            )
        };
        let before = format!("{cursor:?}");
        assert!(!merge_unsaved_debt(
            &mut cursor,
            make_snapshot(),
            now,
            1_000_000
        ));
        assert_eq!(
            format!("{cursor:?}"),
            before,
            "retry merge must roll back too"
        );
        assert!(merge_unsaved_debt(
            &mut cursor,
            make_snapshot(),
            now + Duration::from_secs(1),
            2_000_000,
        ));
        assert_eq!(cursor.retries.len(), 1);
        assert_eq!(cursor.retries[0].id, 90_000);
        assert_eq!(
            cursor.conflict_backoff.iter().copied().collect::<Vec<_>>(),
            [(80_000, now + MAX_RETRY_BACKOFF)]
        );
    }

    #[test]
    fn repeated_dirty_imports_do_not_renew_retry_deadlines_after_clock_reversal() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        let mut durable = ReconcileCursor::default();
        durable.defer_transient(7, now);
        durable.defer_transient(7, now);
        durable
            .conflict_backoff
            .push_back((19, now + MAX_RETRY_BACKOFF));
        let make_snapshot = || {
            Snapshot::capture(
                Binding::at(&database, &archive, GENERATION).unwrap(),
                &durable,
                now,
                9_000_000,
            )
        };
        let mut cursor = ReconcileCursor::default();
        cursor.defer_transient(7, now);
        cursor.defer_transient(7, now);
        let deadline = cursor.retries[0].ready_at;
        for seconds in 0..=5 {
            assert!(merge_unsaved_debt(
                &mut cursor,
                make_snapshot(),
                now + Duration::from_secs(seconds),
                1_000_000,
            ));
            assert_eq!(cursor.retries[0].ready_at, deadline);
            assert_eq!(cursor.retries[0].attempts, 1);
            assert!(cursor.conflict_backoff.is_empty());
        }
        assert!(deadline < now + Duration::from_secs(5));
    }

    #[test]
    fn resumed_worker_repairs_both_cursors_debt_before_history_wrap() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let now = Instant::now();
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            cursor.source_identity = pool.sqlite_identity_key();
            cursor.tail_after = Some(940);
            cursor.backfill_ceiling = Some(0);
            cursor.defer_transient(901, now);
            let first = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            first.save(&cursor).unwrap();
            cursor.defer_transient(903, now);
            assert!(
                first
                    .save_with(&cursor, || Err(io::Error::other(
                        "checkpoint publication failed"
                    )))
                    .is_err()
            );
            cursor.checkpoint_dirty = true;
            drop(first);

            let second = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            let mut other = ReconcileCursor {
                tail_after: Some(940),
                backfill_ceiling: Some(0),
                ..ReconcileCursor::default()
            };
            other.defer_transient(902, now);
            second.save(&other).unwrap();
            drop(second);

            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert!(report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_eq!(
                (report.scanned, report.repaired, report.deferred),
                (3, 3, 0)
            );
            assert!(cursor.retries.is_empty());
            for id in [901, 902, 903] {
                assert_message_committed(cx, pool, config, id, 40);
            }
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 40);
            let rows = conn.query_sync(
                "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id IN (901, 902, 903)", &[],
            ).unwrap();
            assert_eq!(rows.len(), 6);
            for row in rows {
                assert_eq!(row.get_named::<i64>("read_ts").unwrap(), 2_000_000);
                assert_eq!(row.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
            }
        });
    }

    #[test]
    fn worker_at_union_capacity_repairs_without_overwriting_the_other_checkpoint() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let now = Instant::now();
            let store = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            let mut other = ReconcileCursor::default();
            other.defer_transient(901, now);
            store.save(&other).unwrap();
            let path = store.path.clone();
            let before = std::fs::read(&path).unwrap();
            drop(store);
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            cursor.source_identity = pool.sqlite_identity_key();
            cursor.tail_after = Some(901);
            cursor.backfill_ceiling = Some(0);
            cursor.checkpoint_dirty = true;
            for id in 10_000..10_000 + i64::try_from(MAX_RETRY_IDS).unwrap() {
                cursor.defer_transient(id, now);
            }
            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert!(!report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(false));
            assert!(
                report.scanned > 0,
                "bounded repair must still run at checkpoint saturation"
            );
            assert!(report.scanned <= super::super::MAX_IDS_PER_PASS);
            assert!(cursor.checkpoint_dirty);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            let reopened = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            assert_eq!(reopened.load().unwrap().unwrap().retries[0].id, 901);
        });
    }

    #[test]
    fn gh354_occupied_checkpoint_skips_source_probes_and_preserves_bytes() {
        let (_temp, database, archive) = fixture();
        let owner = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        owner.save(&debt(Instant::now())).unwrap();
        let checkpoint = std::fs::read(&owner.path).unwrap();
        let lock_bytes = std::fs::read(&owner.lock_path).unwrap();
        let mut probes = 0;
        for _ in 0..60 {
            let result = with_available_checkpoint(&database, &archive, || {
                probes += 1;
                Ok(())
            });
            assert_eq!(result.unwrap_err(), CHECKPOINT_OWNED);
        }
        assert_eq!(
            probes, 0,
            "a refused checkpoint must not acquire/query the pool"
        );
        assert_eq!(std::fs::read(&owner.path).unwrap(), checkpoint);
        assert_eq!(std::fs::read(&owner.lock_path).unwrap(), lock_bytes);
        drop(owner);
        with_available_checkpoint(&database, &archive, || {
            probes += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(probes, 1, "release must admit the next source probe");
        let reopened = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        assert_eq!(std::fs::read(&reopened.path).unwrap(), checkpoint);
    }

    #[test]
    fn gh354_missing_checkpoint_probe_never_creates_a_sidecar() {
        let (_temp, database, archive) = fixture();
        let binding = Binding::at(&database, &archive, GENERATION).unwrap();
        let path = binding.path().unwrap();
        let lock_path = path.with_extension("json.lock");
        let result =
            with_available_checkpoint::<()>(
                &database,
                &archive,
                || Err("query-only source".into()),
            );
        assert_eq!(result.unwrap_err(), "query-only source");
        assert!(!path.exists());
        assert!(
            !lock_path.exists(),
            "refusal must not create even a lock file"
        );
        with_available_checkpoint(&database, &archive, || Ok(())).unwrap();
        assert!(!path.exists());
        assert!(
            !lock_path.exists(),
            "the hint never creates publication authority"
        );
        let store = Store::open(binding).unwrap();
        assert!(
            store.lock_path.exists(),
            "only authorized Store::open creates the lock"
        );
        assert!(!store.path.exists());
    }

    #[test]
    fn gh354_successful_probe_does_not_bypass_a_racing_authoritative_owner() {
        let (_temp, database, archive) = fixture();
        drop(Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap());
        let mut competitor = None;
        let generation = with_available_checkpoint(&database, &archive, || {
            // The probe must have released its descriptor. Source authorization
            // may take time; another worker can win the real checkpoint now.
            competitor =
                Some(Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap());
            Ok(GENERATION)
        })
        .unwrap();
        let refused = Store::open(Binding::at(&database, &archive, generation).unwrap())
            .err()
            .expect("the original authoritative acquisition must still run");
        assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
        assert!(!competitor.as_ref().unwrap().path.exists());
        drop(competitor);
        assert!(Store::open(Binding::at(&database, &archive, generation).unwrap()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn gh354_unsafe_probe_never_bypasses_source_or_store_validation() {
        for hardlink in [false, true] {
            let (_temp, database, archive) = fixture();
            let path = Binding::at(&database, &archive, GENERATION)
                .unwrap()
                .path()
                .unwrap();
            let lock_path = path.with_extension("json.lock");
            let evidence = archive.join("preserved-evidence");
            std::fs::write(&evidence, b"do not replace").unwrap();
            if hardlink {
                std::fs::hard_link(&evidence, &lock_path).unwrap();
            } else {
                std::os::unix::fs::symlink(&evidence, &lock_path).unwrap();
            }
            let mut probes = 0;
            let result = with_available_checkpoint::<()>(&database, &archive, || {
                probes += 1;
                Err("source authorization refused".into())
            });
            assert_eq!(result.unwrap_err(), "source authorization refused");
            assert_eq!(
                probes, 1,
                "unavailable hints do not authorize source access"
            );
            // A successful source check still cannot legitimize an unsafe lock.
            with_available_checkpoint(&database, &archive, || Ok(())).unwrap();
            assert!(Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).is_err());
            assert_eq!(std::fs::read(&evidence).unwrap(), b"do not replace");
            assert!(!path.exists());
        }
    }

    #[test]
    fn gh354_probe_uses_the_canonical_source_and_archive_namespace() {
        let (_temp, database, archive) = fixture();
        let owner = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let alias = database.parent().unwrap().join(".").join("mail.sqlite3");
        let result = with_available_checkpoint::<()>(&alias, &archive.join("."), || {
            panic!("an alias must not bypass the occupied checkpoint")
        });
        assert_eq!(result.unwrap_err(), CHECKPOINT_OWNED);
        let other_archive = archive.with_file_name("other-archive");
        std::fs::create_dir(&other_archive).unwrap();
        with_available_checkpoint(&database, &other_archive, || Ok(())).unwrap();
        owner.check_lock().unwrap();
    }

    #[test]
    fn gh354_real_query_only_source_cannot_create_or_modify_checkpoint_files() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let path = Binding::capture(cx, pool, config).unwrap().path().unwrap();
            let lock_path = path.with_extension("json.lock");
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            for existing in [false, true] {
                if existing {
                    let owner = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
                    owner.save(&debt(Instant::now())).unwrap();
                }
                let checkpoint = std::fs::read(&path).ok();
                let lock_bytes = std::fs::read(&lock_path).ok();
                let mut cursor = debt(Instant::now());
                let before = format!("{cursor:?}");
                let error =
                    run(cx, &readonly, config, &mut cursor, &AtomicBool::new(false)).unwrap_err();
                assert!(error.contains("query-only snapshots"), "{error}");
                assert_eq!(format!("{cursor:?}"), before);
                assert_eq!(std::fs::read(&path).ok(), checkpoint);
                assert_eq!(std::fs::read(&lock_path).ok(), lock_bytes);
                if !existing {
                    assert!(!path.exists());
                    assert!(!lock_path.exists());
                }
            }
        });
    }

    #[test]
    fn gh354_real_contention_refuses_before_pool_wait_then_recovers_committed_mail() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            cursor.source_identity = pool.sqlite_identity_key();
            cursor.tail_after = Some(930);
            cursor.backfill_ceiling = Some(920);
            cursor.next_lane_is_retry = true;
            cursor.defer_transient(901, Instant::now());
            let owner = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            owner.save(&cursor).unwrap();
            let checkpoint = std::fs::read(&owner.path).unwrap();
            let before = format!("{cursor:?}");
            let stop = AtomicBool::new(false);

            // This fixture has ONE connection. The refusal must not wait for
            // the held borrower or turn into an acquire timeout/source failure.
            let borrower = outcome(block_on(pool.acquire(cx))).unwrap();
            let refused = run(cx, pool, config, &mut cursor, &stop);
            drop(borrower);
            assert_eq!(refused.unwrap_err(), CHECKPOINT_OWNED);
            for _ in 0..60 {
                assert_eq!(
                    run(cx, pool, config, &mut cursor, &stop).unwrap_err(),
                    CHECKPOINT_OWNED
                );
            }
            assert_eq!(
                format!("{cursor:?}"),
                before,
                "refusal cannot reset retry debt"
            );
            assert_eq!(std::fs::read(&owner.path).unwrap(), checkpoint);
            drop(owner);

            let report = run(cx, pool, config, &mut cursor, &stop).unwrap();
            assert!(report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_eq!(report.repaired, super::super::MAX_REPAIRS_PER_BATCH);
            assert!(cursor.retries.is_empty());
            // Uses real committed Git blobs and verifies the source row count
            // and both recipients' read/ACK values, not just created files.
            assert_message_committed(cx, pool, config, 901, 40);
        });
    }

    #[test]
    fn same_live_cursor_rescans_after_between_pass_logical_generation_change() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let (store, mut cursor) = seed_obsolete_progress(cx, pool, config);
            change_logical_generation(cx, pool, &store.binding.database_generation);
            assert_eq!(cursor.source_identity, pool.sqlite_identity_key());
            drop(store);
            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert!(!report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_eq!(report.repaired, super::super::MAX_REPAIRS_PER_BATCH);
            assert_message_committed(cx, pool, config, 940, 40);
        });
    }

    #[test]
    fn obsolete_checkpoint_resets_scan_but_cannot_discard_memory_only_retry_debt() {
        let (_temp, database, archive) = fixture();
        let old = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let now = Instant::now();
        old.save(&debt(now)).unwrap();
        drop(old);
        let current = Store::open(Binding::at(&database, &archive, "11223344").unwrap()).unwrap();
        let mut cursor = ReconcileCursor::settled_before(999);
        cursor.source_identity = "live pool".into();
        cursor.tail_after = Some(100_000);
        cursor.backfill_ceiling = Some(100);
        cursor.startup_backfill_complete = true;
        cursor.startup_tail = Some(super::super::StartupTail {
            after: 100_000,
            ceiling: 110_000,
        });
        cursor.startup_tail_initialized = true;
        cursor.checkpoint_dirty = true;
        cursor.defer_transient(77, now);
        assert!(current.restore(&mut cursor).is_err());
        assert_eq!((cursor.tail_after, cursor.backfill_ceiling), (None, None));
        assert!(!cursor.startup_backfill_complete);
        assert!(cursor.startup_tail.is_none());
        assert!(!cursor.startup_tail_initialized);
        assert_eq!(cursor.retries.len(), 1);
        assert_eq!(cursor.retries[0].id, 77);
        assert_eq!(cursor.retries[0].ready_at, now);
        assert!(cursor.checkpoint_dirty);
        assert_eq!(cursor.source_identity, "live pool");
        assert_eq!(cursor.settled_before_us, 999);
        current.save(&cursor).unwrap();
        let saved = current.load().unwrap().unwrap();
        assert_eq!(saved.binding.database_generation, "11223344");
        assert_eq!(saved.retries.len(), 1);
        assert_eq!(saved.retries[0].id, 77);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_lock_refuses_old_owner_reads_and_writes() {
        let (_temp, database, archive) = fixture();
        let old = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        old.save(&debt(Instant::now())).unwrap();
        let before = std::fs::read(&old.path).unwrap();
        std::fs::rename(
            &old.lock_path,
            old.lock_path.with_extension("retained-lock"),
        )
        .unwrap();
        let current = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        assert!(
            old.load().is_err(),
            "old flock is not ownership of the new path"
        );
        assert!(old.save(&ReconcileCursor::default()).is_err());
        assert_eq!(std::fs::read(&old.path).unwrap(), before);
        assert_eq!(current.load().unwrap().unwrap().tail_after, Some(90_000));
    }

    #[cfg(unix)]
    #[test]
    fn lock_replacement_during_real_staging_prevents_stale_checkpoint_publication() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        store.save(&debt(Instant::now())).unwrap();
        let before = std::fs::read(&store.path).unwrap();
        let mut replacement = None;
        let result = store.save_with(&ReconcileCursor::default(), || {
            std::fs::rename(
                &store.lock_path,
                store.lock_path.with_extension("retained-lock"),
            )?;
            replacement = Some(Store::open(Binding::at(&database, &archive, GENERATION)?)?);
            Ok(())
        });
        assert!(
            result.is_err(),
            "publication must recheck ownership after fsync"
        );
        assert_eq!(std::fs::read(&store.path).unwrap(), before);
        assert_eq!(
            replacement.unwrap().load().unwrap().unwrap().retries.len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_links_installed_after_staging_are_preserved_not_replaced() {
        for hardlink in [false, true] {
            let (_temp, database, archive) = fixture();
            let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
            store.save(&debt(Instant::now())).unwrap();
            let original = std::fs::read(&store.path).unwrap();
            let retained = store.path.with_extension("retained-checkpoint");
            let foreign = archive.join("foreign-evidence");
            std::fs::write(&foreign, b"preserve foreign evidence").unwrap();
            let result = store.save_with(&ReconcileCursor::default(), || {
                std::fs::rename(&store.path, &retained)?;
                if hardlink {
                    std::fs::hard_link(&foreign, &store.path)?;
                } else {
                    std::os::unix::fs::symlink(&foreign, &store.path)?;
                }
                Ok(())
            });
            assert!(
                result.is_err(),
                "unsafe destination must be checked after fsync"
            );
            assert_eq!(std::fs::read(&retained).unwrap(), original);
            assert_eq!(
                std::fs::read(&foreign).unwrap(),
                b"preserve foreign evidence"
            );
            assert_eq!(
                std::fs::read(&store.path).unwrap(),
                b"preserve foreign evidence"
            );
            assert!(
                store.load().is_err(),
                "no reading through substituted links"
            );
        }
    }

    #[test]
    fn logical_generation_changes_invalidate_hints_without_replacing_the_file() {
        let (_temp, database, archive) = fixture();
        let before = Binding::at(&database, &archive, GENERATION).unwrap();
        let after = Binding::at(&database, &archive, "11223344").unwrap();
        assert_eq!(before.generation, after.generation, "same physical file");
        assert_eq!(before.path().unwrap(), after.path().unwrap());
        let mut snapshot = Snapshot::capture(before, &debt(Instant::now()), Instant::now(), 1);
        assert!(snapshot.validate(&after).is_err());
        snapshot.binding.database_generation = after.database_generation.clone();
        snapshot.validate(&after).unwrap();
        snapshot.version = 1;
        assert!(
            snapshot.validate(&after).is_err(),
            "old format has no logical authority"
        );
        for generation in ["", "not-hex", " ", &"a".repeat(129)] {
            assert!(Binding::at(&database, &archive, generation).is_err());
        }
    }

    fn seed_obsolete_progress(cx: &Cx, pool: &DbPool, config: &Config) -> (Store, ReconcileCursor) {
        let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
        cursor.source_identity = pool.sqlite_identity_key();
        cursor.tail_after = Some(940);
        cursor.backfill_ceiling = Some(0);
        let store = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
        store.save(&cursor).unwrap();
        (store, cursor)
    }

    fn change_logical_generation(cx: &Cx, pool: &DbPool, old: &str) {
        // This models the generation change performed by in-place recovery,
        // without deleting/replacing the fixture or changing its message rows.
        let replacement = if old == "aabbccdd" {
            "11223344"
        } else {
            "aabbccdd"
        };
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_sync(
            "UPDATE db_identity SET generation_id = ? WHERE singleton = 0",
            &[replacement.into()],
        )
        .unwrap();
    }

    fn assert_message_committed(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        id: i64,
        expected_messages: i64,
    ) {
        let prepared = super::super::prepare_message(cx, pool, id).unwrap();
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let root = std::fs::canonicalize(&config.storage_root).unwrap();
        for path in std::iter::once(&paths.canonical)
            .chain(std::iter::once(&paths.outbox))
            .chain(paths.inbox.iter())
        {
            let absolute = std::fs::canonicalize(path).unwrap();
            let relative = absolute.strip_prefix(&root).unwrap();
            let entry = tree.get_path(relative).unwrap();
            assert_eq!(
                repo.find_blob(entry.id()).unwrap().content(),
                std::fs::read(path).unwrap()
            );
        }
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("n").unwrap(), expected_messages);
        let rows = conn
            .query_sync(
                "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                &[id.into()],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.get_named::<i64>("read_ts").unwrap(), 2_000_000);
            assert_eq!(row.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
        }
    }

    #[test]
    fn in_place_recovery_does_not_restore_the_previous_mailbox_watermarks() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let (store, _) = seed_obsolete_progress(cx, pool, config);
            change_logical_generation(cx, pool, &store.binding.database_generation);
            let current = Binding::capture(cx, pool, config).unwrap();
            assert_eq!(store.binding.generation, current.generation);
            assert_ne!(
                store.binding.database_generation,
                current.database_generation
            );
            drop(store);
            let mut restarted = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let report = run(cx, pool, config, &mut restarted, &AtomicBool::new(false)).unwrap();
            assert!(
                !report.checkpoint_restored,
                "old logical-generation hints are invalid"
            );
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_eq!(report.repaired, super::super::MAX_REPAIRS_PER_BATCH);
            // A restored tail=940/history=0 cursor would inspect no messages
            // in this pass, delaying repair until the next history round.
            assert_message_committed(cx, pool, config, 940, 40);
        });
    }

    #[test]
    fn generation_change_before_publication_preserves_disk_and_resets_live_watermarks() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let (store, mut cursor) = seed_obsolete_progress(cx, pool, config);
            let before = std::fs::read(&store.path).unwrap();
            change_logical_generation(cx, pool, &store.binding.database_generation);
            let stop = AtomicBool::new(false);
            let error = save_current(cx, pool, config, &mut cursor, &stop, &store).unwrap_err();
            assert!(error.contains("source changed"), "{error}");
            assert_eq!(std::fs::read(&store.path).unwrap(), before);
            assert!(cursor.source_identity.is_empty(), "next pass must rescan");
            drop(store);
            let report = run(cx, pool, config, &mut cursor, &stop).unwrap();
            assert!(!report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_message_committed(cx, pool, config, 940, 40);
        });
    }

    #[test]
    fn cancelled_publication_does_not_advance_the_durable_watermark() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let (store, mut cursor) = seed_obsolete_progress(cx, pool, config);
            let before = std::fs::read(&store.path).unwrap();
            cursor.tail_after = Some(999);
            let scope = cursor.source_identity.clone();
            assert!(
                save_current(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(true),
                    &store,
                )
                .is_err()
            );
            assert_eq!(
                cursor.source_identity, scope,
                "cancellation is not source replacement"
            );
            assert_eq!(std::fs::read(&store.path).unwrap(), before);
            assert_eq!(store.load().unwrap().unwrap().tail_after, Some(940));
        });
    }

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
    fn checkpoint_io_errors_identify_the_operation_without_erasing_the_cause() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.sqlite3");
        let error = Binding::at(&missing, temp.path(), GENERATION).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("canonicalize database"));
        let cause = error
            .get_ref()
            .unwrap()
            .downcast_ref::<CheckpointIoError>()
            .unwrap();
        assert_eq!(cause.path, missing);
        assert!(cause.source.raw_os_error().is_some());

        let invalid_path = std::fs::canonicalize(temp.path())
            .unwrap()
            .join("invalid\0checkpoint");
        let error = check_sidecar(&invalid_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("validate path components"));
    }

    // This helper is executed in fresh test processes by the lifecycle test
    // below. Command::env avoids changing the parent test runner's environment.
    #[test]
    fn checkpoint_restart_child() {
        let Some(root) = std::env::var_os("AM_TEST_CHECKPOINT_RESTART_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let database = root.join("mail.sqlite3");
        let archive = root.join("archive");
        let mode = std::env::var("AM_TEST_CHECKPOINT_RESTART_MODE").unwrap();
        if mode == "contend" {
            let refused = with_available_checkpoint(&database, &archive, || {
                panic!("a competing process must not enter source authorization")
            });
            let refused: Result<(), String> = refused;
            assert_eq!(refused.unwrap_err(), CHECKPOINT_OWNED);
            let error = Store::open(Binding::at(&database, &archive, GENERATION).unwrap())
                .err()
                .expect("the authoritative lock must also refuse the contender");
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert!(error.to_string().contains("acquire exclusive lock"));
            std::fs::write(root.join("contender-refused"), b"owned").unwrap();
            return;
        }

        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        #[cfg(windows)]
        assert!(matches!(
            store.path.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        match mode.as_str() {
            "publish" => {
                assert!(store.load().unwrap().is_none());
                let mut cursor = debt(Instant::now());
                store.save(&cursor).unwrap();
                cursor.tail_after = Some(90_001);
                // Exercise replacing an existing checkpoint, then exit without
                // running Store's destructor. Kernel ownership must be released.
                store.save(&cursor).unwrap();
                std::process::exit(0);
            }
            "crash-after-stage" => {
                let mut cursor = debt(Instant::now());
                cursor.tail_after = Some(100_000);
                cursor.retries.clear();
                store.save_with(&cursor, || std::process::exit(77)).unwrap();
                panic!("the child must exit after fsync and before publication");
            }
            "restore" => {
                let mut cursor = ReconcileCursor::settled_before(999);
                cursor.source_identity = "fresh process".into();
                assert_eq!(store.restore(&mut cursor).unwrap(), RestoreState::Restored);
                assert_eq!(cursor.tail_after, Some(90_001));
                assert_eq!(cursor.backfill_ceiling, Some(200));
                assert!(cursor.next_lane_is_history && cursor.next_lane_is_retry);
                assert_eq!(
                    cursor
                        .retries
                        .iter()
                        .map(|retry| retry.id)
                        .collect::<Vec<_>>(),
                    [7, 80_000]
                );
                assert_eq!(cursor.retries[1].attempts, 6);
                assert_eq!(cursor.settled_before_us, 999);
                assert_eq!(cursor.source_identity, "fresh process");
                store.save(&cursor).unwrap();
                std::fs::write(root.join("restart-restored"), b"90001:7,80000").unwrap();
            }
            _ => panic!("unexpected checkpoint child mode: {mode}"),
        }
    }

    #[test]
    fn checkpoint_recovers_after_process_exit_during_staged_publication() {
        let (temp, database, archive) = fixture();
        let path = Binding::at(&database, &archive, GENERATION)
            .unwrap()
            .path()
            .unwrap();
        let run_child = |mode: &str, expected_code: i32| {
            let module = module_path!().split_once("::").unwrap().1;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("{module}::checkpoint_restart_child"),
                    "--nocapture",
                ])
                .env("AM_TEST_CHECKPOINT_RESTART_ROOT", temp.path())
                .env("AM_TEST_CHECKPOINT_RESTART_MODE", mode)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected_code),
                "{mode}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        assert!(!path.exists());
        assert!(!path.with_extension("json.lock").exists());
        run_child("publish", 0);
        assert!(path.is_file());
        assert!(path.with_extension("json.lock").is_file());
        let published = std::fs::read(&path).unwrap();
        let owner = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        run_child("contend", 0);
        assert_eq!(
            std::fs::read(temp.path().join("contender-refused")).unwrap(),
            b"owned"
        );
        assert_eq!(std::fs::read(&path).unwrap(), published);
        drop(owner);
        run_child("crash-after-stage", 77);
        assert_eq!(std::fs::read(&path).unwrap(), published);
        run_child("restore", 0);
        assert_eq!(
            std::fs::read(temp.path().join("restart-restored")).unwrap(),
            b"90001:7,80000"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_atomic_replacement_preserves_the_last_checkpoint() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let mut cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        let published = std::fs::read(&store.path).unwrap();
        // Permit metadata and reads but deny deletion/rename of this exact
        // destination. This must exercise the real failed persist operation.
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(&store.path)
            .unwrap();
        cursor.tail_after = Some(90_001);
        let error = store.save(&cursor).unwrap_err();
        assert!(
            error.to_string().contains("publish staged snapshot"),
            "{error}"
        );
        assert_eq!(std::fs::read(&store.path).unwrap(), published);
        assert_eq!(store.load().unwrap().unwrap().tail_after, Some(90_000));
        assert_eq!(cursor.retries.len(), 2);
        drop(blocker);
        store.save(&cursor).unwrap();
        drop(store);
        let reopened = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        assert_eq!(reopened.load().unwrap().unwrap().tail_after, Some(90_001));
    }

    #[cfg(windows)]
    #[test]
    fn windows_checkpoint_rejects_linked_sidecars_and_lock_paths() {
        for lock_link in [false, true] {
            let (_temp, database, archive) = fixture();
            let binding = Binding::at(&database, &archive, GENERATION).unwrap();
            let path = binding.path().unwrap();
            let linked = if lock_link {
                path.with_extension("json.lock")
            } else {
                path.clone()
            };
            let evidence = archive.join("preserved-evidence");
            std::fs::write(&evidence, b"do not modify").unwrap();
            std::os::windows::fs::symlink_file(&evidence, &linked).unwrap();
            assert!(
                std::fs::symlink_metadata(&linked)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the native symlink fixture must exist"
            );
            assert!(Store::open(binding).is_err());
            assert_eq!(std::fs::read(&evidence).unwrap(), b"do not modify");
            assert!(
                std::fs::symlink_metadata(&linked)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_link_installed_after_staging_is_not_followed_or_replaced() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        store.save(&debt(Instant::now())).unwrap();
        let original = std::fs::read(&store.path).unwrap();
        let retained = store.path.with_extension("retained-checkpoint");
        let evidence = archive.join("foreign-evidence");
        std::fs::write(&evidence, b"preserve foreign evidence").unwrap();
        let error = store
            .save_with(&ReconcileCursor::default(), || {
                std::fs::rename(&store.path, &retained)?;
                std::os::windows::fs::symlink_file(&evidence, &store.path)?;
                assert!(
                    std::fs::symlink_metadata(&store.path)?
                        .file_type()
                        .is_symlink(),
                    "the native symlink fixture must exist"
                );
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("symlinked component"), "{error}");
        assert_eq!(std::fs::read(&retained).unwrap(), original);
        assert_eq!(
            std::fs::read(&evidence).unwrap(),
            b"preserve foreign evidence"
        );
        assert!(
            std::fs::symlink_metadata(&store.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(store.load().is_err(), "the substituted link is never read");
    }

    #[test]
    fn checkpoint_restores_low_and_high_retry_debt_and_lane_turns_together() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        let binding = Binding::at(&database, &archive, GENERATION).unwrap();
        let snapshot = Snapshot::capture(binding, &debt(now), now, 1_000_000);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let restored: Snapshot = serde_json::from_slice(&bytes).unwrap();
        restored
            .validate(&Binding::at(&database, &archive, GENERATION).unwrap())
            .unwrap();
        let mut cursor = ReconcileCursor::settled_before(999);
        cursor.source_identity = "new process pool".into();
        restored.restore(&mut cursor, now, 11_000_000);
        assert_eq!(cursor.tail_after, Some(90_000));
        assert_eq!(cursor.backfill_ceiling, Some(200));
        assert!(cursor.next_lane_is_history);
        assert!(cursor.next_lane_is_retry);
        assert_eq!(
            cursor
                .retries
                .iter()
                .map(|retry| retry.id)
                .collect::<Vec<_>>(),
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
                Binding::at(&database, &archive, GENERATION).unwrap(),
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
        let binding = Binding::at(&database, &archive, GENERATION).unwrap();
        let now = Instant::now();
        for change in [
            "version",
            "negative",
            "duplicate",
            "cross_lane",
            "delay",
            "count",
        ] {
            let mut snapshot = Snapshot::capture(
                Binding::at(&database, &archive, GENERATION).unwrap(),
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
        let before = Binding::at(&database, &archive, GENERATION).unwrap();
        std::fs::write(&database, b"an ordinary database write changes its length").unwrap();
        assert_eq!(
            before,
            Binding::at(&database, &archive, GENERATION).unwrap()
        );
        let other = archive.with_file_name("other-archive");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(
            before.path().unwrap(),
            Binding::at(&database, &other, GENERATION)
                .unwrap()
                .path()
                .unwrap()
        );
        std::fs::rename(&database, database.with_extension("held")).unwrap();
        std::fs::write(&database, b"replacement generation").unwrap();
        assert_ne!(
            before,
            Binding::at(&database, &archive, GENERATION).unwrap()
        );
    }

    #[test]
    fn checkpoint_publish_survives_reopening_and_rejects_stale_source() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        assert!(store.load().unwrap().is_none());
        store.save(&debt(Instant::now())).unwrap();
        let bytes = std::fs::read(&store.path).unwrap();
        drop(store);
        let reopened = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
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
        let first = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        first.save(&debt(Instant::now())).unwrap();
        let bytes = std::fs::read(&first.path).unwrap();
        let error = Store::open(Binding::at(&database, &archive, GENERATION).unwrap())
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
        drop(first);
        assert!(Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).is_ok());
    }

    #[test]
    fn checkpoint_reads_reject_malformed_and_oversized_records() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
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
        let store = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let mut cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        // Simulate a crash between advancing the in-memory scan and publishing
        // its checkpoint. The old high-water mark and its debt remain paired.
        cursor.tail_after = Some(100_000);
        cursor.retries.clear();
        drop(store);
        let restarted = Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).unwrap();
        let saved = restarted.load().unwrap().unwrap();
        assert_eq!(saved.tail_after, Some(90_000));
        assert_eq!(
            saved
                .retries
                .iter()
                .map(|retry| retry.id)
                .collect::<Vec<_>>(),
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
            let store = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
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
    fn restarted_worker_prioritizes_new_crash_orphans_without_discarding_old_progress() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            // A slow pre-crash worker checkpointed far below the newest rows.
            // Nearly all intervening mail reached Git through ordinary writes;
            // the newest two accepted messages died with that process's WBQ.
            for id in 902..939 {
                if id != 903 {
                    let prepared = super::super::prepare_message(cx, pool, id).unwrap();
                    super::super::reconcile_prepared(config, &prepared).unwrap();
                }
            }
            let mut previous = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            previous.source_identity = pool.sqlite_identity_key();
            previous.tail_after = Some(905);
            previous.backfill_ceiling = Some(904);
            previous.next_lane_is_retry = true;
            previous.defer_transient(901, Instant::now());
            previous.retries.push_back(DeferredRetry {
                id: 902,
                attempts: 3,
                ready_at: Instant::now() + MAX_RETRY_BACKOFF,
            });
            let store = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            store.save(&previous).unwrap();
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
            assert_eq!(report.repaired, 4);
            assert_eq!(report.deferred, 0);
            assert!(report.scanned <= super::super::MAX_IDS_PER_PASS);
            assert!(report.payload_bytes <= super::super::MAX_BATCH_PAYLOAD_BYTES);
            for id in [901, 903, 939, 940] {
                assert_message_committed(cx, pool, config, id, 40);
            }
            // Recent coverage must not falsely certify the intervening range
            // or restart history at its newest ID. The old cooldown survives.
            assert!(restarted.tail_after.is_some_and(|id| id > 905 && id < 939));
            assert!(restarted.backfill_ceiling.is_some_and(|id| id < 904));
            let store = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            let saved = store.load().unwrap().unwrap();
            assert_eq!(saved.tail_after, restarted.tail_after);
            assert_eq!(saved.backfill_ceiling, restarted.backfill_ceiling);
            assert_eq!(saved.retries.len(), 1);
            assert_eq!(saved.retries[0].id, 902);
            assert_eq!(saved.retries[0].attempts, 3);
            assert!(saved.retries[0].wait_us > 0);
        });
    }

    #[test]
    fn fresh_mailbox_checkpoints_without_requiring_a_reservation_first() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert!(
                conn.query_sync("SELECT generation_id FROM db_identity", &[])
                    .unwrap()
                    .is_empty()
            );
            drop(conn);

            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let report = super::super::reconcile_message_batch(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 1);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_message_committed(cx, pool, config, 901, 1);
            let first = Binding::capture(cx, pool, config).unwrap();
            assert!(first.path().unwrap().is_file());
            assert!(!first.database_generation.is_empty());
            assert!(
                first
                    .database_generation
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );

            // A new maintenance cursor must reuse both the durable identity and
            // its saved progress, not mint a new generation on every restart.
            let mut restarted = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let resumed = super::super::reconcile_message_batch(
                cx,
                pool,
                config,
                &mut restarted,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(resumed.checkpoint_restored);
            assert_eq!(Binding::capture(cx, pool, config).unwrap(), first);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT generation_id FROM db_identity", &[])
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].get_named::<String>("generation_id").unwrap(),
                first.database_generation
            );
        });
    }

    #[test]
    fn unseeded_query_only_mailbox_cannot_initialize_identity_or_checkpoint() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let database = std::fs::canonicalize(pool.sqlite_path()).unwrap();
            let archive = std::fs::canonicalize(&config.storage_root).unwrap();
            let path = checkpoint_path(&database, &archive).unwrap();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert!(
                conn.query_sync("SELECT generation_id FROM db_identity", &[])
                    .unwrap()
                    .is_empty()
            );
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            drop(conn);
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let error = super::super::reconcile_message_batch(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(error.contains("query-only"), "{error}");
            assert!(!path.exists());
            assert!(!path.with_extension("json.lock").exists());
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert!(
                conn.query_sync("SELECT generation_id FROM db_identity", &[])
                    .unwrap()
                    .is_empty()
            );
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
        });
    }

    #[test]
    fn critical_disk_pressure_leaves_an_unseeded_identity_untouched() {
        super::super::tests::retention_fixture(0, |cx, pool, _| {
            let pressure = &mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level;
            let previous = pressure.load();
            pressure.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
            let result = source_generation(cx, pool);
            pressure.set(previous);
            assert!(result.unwrap_err().contains("critical disk pressure"));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert!(
                conn.query_sync("SELECT generation_id FROM db_identity", &[])
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn stopped_worker_does_not_create_a_progress_sidecar() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let path = Binding::capture(cx, pool, config).unwrap().path().unwrap();
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
        let binding = Binding::at(&database, &archive, GENERATION).unwrap();
        let path = binding.path().unwrap();
        let evidence = archive.join("preserved-evidence");
        std::fs::write(&evidence, b"do not replace").unwrap();
        std::os::unix::fs::symlink(&evidence, &path).unwrap();
        assert!(Store::open(binding).is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"do not replace");
        std::fs::rename(&path, path.with_extension("held-symlink")).unwrap();
        std::fs::hard_link(&evidence, &path).unwrap();
        assert!(Store::open(Binding::at(&database, &archive, GENERATION).unwrap()).is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"do not replace");
    }
}
