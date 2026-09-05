//! Generation-consistent, process-wide archive-backed read snapshots.
//!
//! Degraded mailbox reads are expensive: they replay the Git archive and merge
//! readable live-only state into a disposable `SQLite` database.  This module
//! gives every tool and resource in a process one owner for that work.  A
//! snapshot is published only after indexed archive markers and exact live
//! database generations taken before and after reconstruction agree, and its
//! pool is incapable of mutation.

use asupersync::Cx;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File, Metadata};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REGISTRY_CAPACITY: usize = 16;
const WORKER_LIMIT: usize = 4;
const BUILD_TIMEOUT: Duration = Duration::from_mins(2);
const WAIT_SLICE: Duration = Duration::from_millis(25);
const EXACT_AUDIT_INTERVAL: Duration = Duration::from_secs(1);
const GENERATION_RETRIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    Cancelled,
    Busy(String),
    TimedOut(String),
    Failed(String),
}

impl AcquireError {
    fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    storage_root: PathBuf,
    sqlite_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Generation {
    archive: mcp_agent_mail_storage::ArchiveReadGeneration,
    live: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheapGeneration {
    archive: mcp_agent_mail_storage::ArchiveReadGeneration,
    live: [u8; 32],
}

pub struct SharedSnapshot {
    pool: mcp_agent_mail_db::DbPool,
    _directory: mcp_agent_mail_db::pool::CanonicalSnapshotTempDir,
}

impl SharedSnapshot {
    pub(crate) fn pool(&self) -> mcp_agent_mail_db::DbPool {
        self.pool.clone()
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        Path::new(self.pool.sqlite_path())
    }
}

#[derive(Clone)]
enum Decision {
    Live,
    Snapshot(Arc<SharedSnapshot>),
}

impl Decision {
    fn snapshot(&self) -> Option<Arc<SharedSnapshot>> {
        match self {
            Self::Live => None,
            Self::Snapshot(snapshot) => Some(Arc::clone(snapshot)),
        }
    }
}

struct Ready {
    generation: Generation,
    cheap: CheapGeneration,
    decision: Decision,
    exact_at: Instant,
    invalidation_epoch: u64,
    writer_epoch: u64,
    archive_epoch: u64,
}

#[derive(Debug)]
struct Completion {
    result: Mutex<Option<Result<(), AcquireError>>>,
}

impl Completion {
    const fn pending() -> Self {
        Self {
            result: Mutex::new(None),
        }
    }

    fn finish(&self, result: Result<(), AcquireError>) {
        let mut stored = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stored.is_none() {
            *stored = Some(result);
        }
    }

    fn result(&self) -> Option<Result<(), AcquireError>> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone)]
struct Build {
    token: u64,
    invalidation_epoch: u64,
    writer_epoch: u64,
    archive_epoch: u64,
    deadline: Instant,
    completion: Arc<Completion>,
}

#[derive(Default)]
struct SlotState {
    ready: Option<Ready>,
    building: Option<Build>,
    next_token: u64,
    writers: usize,
}

struct Slot {
    scope: Scope,
    state: Mutex<SlotState>,
    invalidation_epoch: AtomicU64,
    /// Wakes waiters parked in [`wait_for_build`] / [`wait_retry_slice`].
    ///
    /// This is a `std::sync::Condvar` (paired with `state`) and NOT an async
    /// `Notify` + runtime-timer timeout, deliberately (GH#203). The gate is
    /// reached through fastmcp's sync dispatch bridge, which drives tool
    /// futures via nested `block_on` calls on a private current-thread
    /// runtime. In that topology the runtime timer wheel is never pumped, so
    /// an `asupersync::time::timeout(WAIT_SLICE, notified)` that misses its
    /// notify parks forever — the deadline checks above it can never re-run.
    /// A condvar `wait_timeout` bounds every park with an OS timer instead,
    /// so a missed or never-sent wakeup (e.g. the storage-crate WBQ mutation
    /// counters, which have no notify path into this module) costs one
    /// `WAIT_SLICE`, not a hang.
    wake: std::sync::Condvar,
    #[cfg(test)]
    reconstructions: AtomicUsize,
    #[cfg(test)]
    reconstructions_active: AtomicUsize,
    #[cfg(test)]
    reconstructions_max_active: AtomicUsize,
}

impl Slot {
    fn new(scope: Scope) -> Self {
        Self {
            scope,
            state: Mutex::new(SlotState::default()),
            invalidation_epoch: AtomicU64::new(0),
            wake: std::sync::Condvar::new(),
            #[cfg(test)]
            reconstructions: AtomicUsize::new(0),
            #[cfg(test)]
            reconstructions_active: AtomicUsize::new(0),
            #[cfg(test)]
            reconstructions_max_active: AtomicUsize::new(0),
        }
    }
}

#[derive(Default)]
struct Registry {
    slots: VecDeque<Arc<Slot>>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
static WORKERS_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static WRITERS_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static WRITER_EPOCH: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static TEST_BUILD_DELAY_MS: AtomicU64 = AtomicU64::new(0);

fn canonicalish(path: &Path) -> Result<PathBuf, AcquireError> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(AcquireError::failed)
}

fn scope(storage_root: &Path, sqlite_path: &Path) -> Result<Scope, AcquireError> {
    Ok(Scope {
        storage_root: canonicalish(storage_root)?,
        sqlite_path: canonicalish(sqlite_path)?,
    })
}

fn slot_for(scope: Scope) -> Result<Arc<Slot>, AcquireError> {
    let mut registry = REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = registry.slots.iter().position(|slot| slot.scope == scope) {
        let slot = registry
            .slots
            .remove(index)
            .expect("located snapshot slot must remain present");
        registry.slots.push_back(Arc::clone(&slot));
        return Ok(slot);
    }

    let retired = if registry.slots.len() >= REGISTRY_CAPACITY {
        let Some(index) = registry.slots.iter().position(|candidate| {
            Arc::strong_count(candidate) == 1 && {
                let state = candidate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.building.is_none()
                    && state.writers == 0
                    && state
                        .ready
                        .as_ref()
                        .is_none_or(|ready| match &ready.decision {
                            Decision::Live => true,
                            Decision::Snapshot(snapshot) => Arc::strong_count(snapshot) == 1,
                        })
            }
        }) else {
            return Err(AcquireError::Busy(format!(
                "archive-read snapshot registry reached its hard capacity of {REGISTRY_CAPACITY} active mailbox scopes"
            )));
        };
        registry.slots.remove(index)
    } else {
        None
    };

    let slot = Arc::new(Slot::new(scope));
    registry.slots.push_back(Arc::clone(&slot));
    drop(registry);
    drop(retired);
    Ok(slot)
}

/// A lease spanning a live mutation path.  Entering and leaving both advance
/// the generation, so a reconstruction can never publish across a write.
pub struct WriteGuard {
    slot: Option<Arc<Slot>>,
    active: bool,
    /// #219: registers this write with the db-crate promotion barrier so a
    /// live-file recovery promotion can never rename the database out from
    /// under this call. Acquisition blocks while a promotion is in flight.
    _write_activity: mcp_agent_mail_db::write_barrier::WriteActivityGuard,
}

impl WriteGuard {
    pub(crate) fn begin(storage_root: &Path, sqlite_path: Option<&Path>) -> Self {
        let write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
        WRITERS_ACTIVE.fetch_add(1, Ordering::AcqRel);
        WRITER_EPOCH.fetch_add(1, Ordering::AcqRel);
        let slot = sqlite_path
            .and_then(|sqlite_path| scope(storage_root, sqlite_path).ok())
            .and_then(|scope| slot_for(scope).ok());
        if let Some(slot) = &slot {
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.writers = state.writers.saturating_add(1);
            slot.invalidation_epoch.fetch_add(1, Ordering::AcqRel);
            drop(state);
            slot.wake.notify_all();
        }
        Self {
            slot,
            active: true,
            _write_activity: write_activity,
        }
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(slot) = &self.slot {
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.writers = state.writers.saturating_sub(1);
            slot.invalidation_epoch.fetch_add(1, Ordering::AcqRel);
            drop(state);
        }
        WRITER_EPOCH.fetch_add(1, Ordering::AcqRel);
        WRITERS_ACTIVE.fetch_sub(1, Ordering::AcqRel);
        if let Some(slot) = &self.slot {
            slot.wake.notify_all();
        }
        self.active = false;
    }
}

fn modified_parts(metadata: &Metadata) -> (u64, u32) {
    metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .map_or((0, 0), |value| (value.as_secs(), value.subsec_nanos()))
}

fn check_deadline(deadline: Instant, phase: &str) -> Result<(), AcquireError> {
    if Instant::now() >= deadline {
        Err(AcquireError::TimedOut(format!(
            "archive-read snapshot {phase} exceeded its {} second build deadline",
            BUILD_TIMEOUT.as_secs()
        )))
    } else {
        Ok(())
    }
}

fn hash_missing(hasher: &mut Sha256, path: &Path) {
    hasher.update([0]);
    hasher.update(path.as_os_str().as_encoded_bytes());
}

fn hash_tree(
    root: &Path,
    path: &Path,
    hasher: &mut Sha256,
    exact: bool,
    deadline: Instant,
) -> Result<(), AcquireError> {
    check_deadline(deadline, "generation scan")?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash_missing(hasher, path);
            return Ok(());
        }
        Err(error) => return Err(AcquireError::failed(error)),
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    hasher.update(relative.as_os_str().as_encoded_bytes());
    let file_type = metadata.file_type();
    hasher.update([
        u8::from(file_type.is_file()),
        u8::from(file_type.is_dir()),
        u8::from(file_type.is_symlink()),
    ]);
    hasher.update(metadata.len().to_le_bytes());
    if !exact {
        let (seconds, nanos) = modified_parts(&metadata);
        hasher.update(seconds.to_le_bytes());
        hasher.update(nanos.to_le_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            hasher.update(metadata.dev().to_le_bytes());
            hasher.update(metadata.ino().to_le_bytes());
            hasher.update(metadata.ctime().to_le_bytes());
            hasher.update(metadata.ctime_nsec().to_le_bytes());
        }
    }

    if file_type.is_symlink() {
        return Err(AcquireError::Failed(format!(
            "generation scan refuses symlinked authority input {}",
            path.display()
        )));
    }
    if file_type.is_file() {
        if exact {
            let mut file = File::open(path).map_err(AcquireError::failed)?;
            // Heap, not stack: exact generation scans now also run on the
            // caller's dispatch thread (GH#203 content re-validation), whose
            // stack is already deep with nested block_on frames — a 256 KiB
            // stack array there is a stack-overflow abort (GH#202 class).
            let mut buffer = vec![0_u8; 256 * 1024];
            loop {
                check_deadline(deadline, "generation scan")?;
                let read = file.read(&mut buffer).map_err(AcquireError::failed)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        }
        return Ok(());
    }
    if !file_type.is_dir() {
        return Err(AcquireError::Failed(format!(
            "unsupported archive artifact type at {}",
            path.display()
        )));
    }

    let mut children = fs::read_dir(path)
        .map_err(AcquireError::failed)?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(AcquireError::failed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    children.sort();
    for child in children {
        hash_tree(root, &child, hasher, exact, deadline)?;
    }
    Ok(())
}

fn hash_optional_file(
    root: &Path,
    path: &Path,
    hasher: &mut Sha256,
    exact: bool,
    deadline: Instant,
) -> Result<(), AcquireError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            hash_tree(root, path, hasher, exact, deadline)
        }
        Ok(_) => Err(AcquireError::Failed(format!(
            "generation input is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash_missing(hasher, path);
            Ok(())
        }
        Err(error) => Err(AcquireError::failed(error)),
    }
}

fn archive_generation(
    scope: &Scope,
) -> Result<mcp_agent_mail_storage::ArchiveReadGeneration, AcquireError> {
    mcp_agent_mail_storage::archive_read_generation(&scope.storage_root).map_err(
        |error| match error {
            mcp_agent_mail_storage::StorageError::LockContention { message }
            | mcp_agent_mail_storage::StorageError::GitIndexLock { message, .. } => {
                AcquireError::Busy(message)
            }
            error => AcquireError::failed(error),
        },
    )
}

/// Byte ranges of the `SQLite` file header that `FrankenSQLite` mutates on every
/// open even when no page changes: the file change counter (24..28) and the
/// version-valid-for counter (92..96). Measured during the GH#203 livelock on
/// Linux: successive `cmp -l` snapshots of the scratch db differ in exactly
/// these four bytes while every page byte is stable, and the file's
/// mtime/ctime bump once per open. They are masked out of the exact live
/// hash so that merely opening the database can never invalidate a published
/// snapshot generation. Real writes always change page bytes outside these
/// ranges, so nothing observable is lost.
const SQLITE_VOLATILE_HEADER_RANGES: [(usize, usize); 2] = [(24, 28), (92, 96)];

/// Exact-content hash of the main `SQLite` file with the open-volatile header
/// counters masked to zero. Mirrors [`hash_tree`]'s exact framing (relative
/// path, type flags, length, content) so the two stay comparable in shape.
fn hash_live_main_exact(
    root: &Path,
    path: &Path,
    hasher: &mut Sha256,
    deadline: Instant,
) -> Result<(), AcquireError> {
    check_deadline(deadline, "generation scan")?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash_missing(hasher, path);
            return Ok(());
        }
        Err(error) => return Err(AcquireError::failed(error)),
    };
    if !metadata.is_file() {
        return Err(AcquireError::Failed(format!(
            "generation input is not a regular file: {}",
            path.display()
        )));
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    hasher.update(relative.as_os_str().as_encoded_bytes());
    hasher.update([1, 0, 0]);
    hasher.update(metadata.len().to_le_bytes());
    let mut file = File::open(path).map_err(AcquireError::failed)?;
    // Heap, not stack: this runs on the caller's dispatch thread (see
    // hash_tree's exact branch for the GH#202 rationale).
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut position: usize = 0;
    loop {
        check_deadline(deadline, "generation scan")?;
        let read = file.read(&mut buffer).map_err(AcquireError::failed)?;
        if read == 0 {
            break;
        }
        for (start, end) in SQLITE_VOLATILE_HEADER_RANGES {
            let lo = start.max(position);
            let hi = end.min(position.saturating_add(read));
            if lo < hi {
                buffer[lo - position..hi - position].fill(0);
            }
        }
        hasher.update(&buffer[..read]);
        position = position.saturating_add(read);
    }
    Ok(())
}

fn hash_live(scope: &Scope, exact: bool, deadline: Instant) -> Result<[u8; 32], AcquireError> {
    let mut hasher = Sha256::new();
    hasher.update(if exact {
        b"agent-mail-live-generation-v1-exact".as_slice()
    } else {
        b"agent-mail-live-generation-v1-cheap".as_slice()
    });
    let root = scope.sqlite_path.parent().unwrap_or_else(|| Path::new("."));
    if exact {
        // The main db's exact hash masks the open-volatile header counters;
        // sidecars are hashed verbatim (opens leave them byte-stable).
        hash_live_main_exact(root, &scope.sqlite_path, &mut hasher, deadline)?;
    } else {
        hash_optional_file(root, &scope.sqlite_path, &mut hasher, false, deadline)?;
    }
    for path in ["-journal", "-wal"].into_iter().map(|suffix| {
        let mut path = scope.sqlite_path.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }) {
        hash_optional_file(root, &path, &mut hasher, exact, deadline)?;
    }
    Ok(hasher.finalize().into())
}

fn exact_generation(scope: &Scope, deadline: Instant) -> Result<Generation, AcquireError> {
    check_deadline(deadline, "archive generation")?;
    Ok(Generation {
        archive: archive_generation(scope)?,
        live: hash_live(scope, true, deadline)?,
    })
}

fn cheap_generation(scope: &Scope, deadline: Instant) -> Result<CheapGeneration, AcquireError> {
    check_deadline(deadline, "archive generation")?;
    Ok(CheapGeneration {
        archive: archive_generation(scope)?,
        live: hash_live(scope, false, deadline)?,
    })
}

fn validate_inventory(
    inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
) -> Result<(), AcquireError> {
    if inventory.parse_errors != 0 {
        return Err(AcquireError::Failed(format!(
            "archive inventory found {} malformed canonical message artifact(s); refusing partial publication",
            inventory.parse_errors
        )));
    }
    Ok(())
}

fn validate_reconstruction(
    inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
    stats: &mcp_agent_mail_db::ReconstructStats,
) -> Result<(), AcquireError> {
    validate_inventory(inventory)?;
    if stats.parse_errors != 0 {
        return Err(AcquireError::Failed(format!(
            "archive reconstruction skipped {} unreadable or malformed artifact(s); refusing partial publication",
            stats.parse_errors
        )));
    }
    let accounted = stats
        .messages
        .checked_add(stats.duplicate_canonical_message_files)
        .ok_or_else(|| AcquireError::Failed("message accounting overflow".to_string()))?;
    let duplicate_events = stats
        .duplicate_canonical_message_files
        .checked_add(stats.cross_project_canonical_collisions)
        .ok_or_else(|| AcquireError::Failed("duplicate accounting overflow".to_string()))?;
    let unexplained_duplicate_ids = inventory
        .duplicate_canonical_message_ids
        .saturating_sub(stats.duplicate_canonical_message_ids);
    if stats.projects != inventory.projects
        || stats.agents != inventory.agents
        || accounted != inventory.canonical_message_files
        || duplicate_events != inventory.duplicate_canonical_message_files
        || stats.duplicate_canonical_message_ids > inventory.duplicate_canonical_message_ids
        || unexplained_duplicate_ids > stats.cross_project_canonical_collisions
    {
        return Err(AcquireError::Failed(format!(
            "archive reconstruction accounting mismatch: inventory projects={} agents={} files={} duplicate_files={} duplicate_ids={}; reconstruction projects={} agents={} messages={} duplicate_files={} duplicate_ids={} cross_project_collisions={}",
            inventory.projects,
            inventory.agents,
            inventory.canonical_message_files,
            inventory.duplicate_canonical_message_files,
            inventory.duplicate_canonical_message_ids,
            stats.projects,
            stats.agents,
            stats.messages,
            stats.duplicate_canonical_message_files,
            stats.duplicate_canonical_message_ids,
            stats.cross_project_canonical_collisions,
        )));
    }
    Ok(())
}

fn live_probe(path: &Path) -> Result<mcp_agent_mail_db::GuardedReadOnlyConn, String> {
    // Engine-dispatching: a live primary that was reconstructed or restored
    // (no namespace pair) is compared against the archive through canonical
    // SQLite instead of being treated as unavailable.
    mcp_agent_mail_db::pool::open_guarded_read_only_sqlite_file(
        path,
        "archive-read live database probe",
    )
    .map_err(|error| error.to_string())
}

fn private_snapshot_probe(path: &Path) -> Result<mcp_agent_mail_db::DbConn, String> {
    let conn = mcp_agent_mail_db::DbConn::open_file_read_only(path.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())?;
    conn.execute_raw("PRAGMA query_only = ON;")
        .map_err(|error| error.to_string())?;
    Ok(conn)
}

fn snapshot_required(
    scope: &Scope,
    database_url: &str,
    inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
) -> Result<bool, AcquireError> {
    let archive_has_state = crate::tool_util::read_archive_inventory_has_state(inventory);
    match live_probe(&scope.sqlite_path) {
        Ok(conn) => {
            if archive_has_state
                && crate::tool_util::live_db_is_suspect(
                    database_url,
                    &scope.storage_root,
                    &scope.sqlite_path,
                )
            {
                return Ok(true);
            }
            crate::tool_util::read_archive_is_ahead(
                &scope.storage_root,
                &scope.sqlite_path,
                &conn,
                inventory,
            )
            .map_err(AcquireError::Failed)
        }
        Err(error) if archive_has_state => {
            tracing::warn!(
                source = %scope.sqlite_path.display(),
                storage_root = %scope.storage_root.display(),
                error = %error,
                "using archive snapshot because the live SQLite source is unavailable"
            );
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

fn build_snapshot(
    slot: &Slot,
    inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
) -> Result<Arc<SharedSnapshot>, AcquireError> {
    #[cfg(test)]
    {
        slot.reconstructions.fetch_add(1, Ordering::Relaxed);
        let active = slot.reconstructions_active.fetch_add(1, Ordering::SeqCst) + 1;
        slot.reconstructions_max_active
            .fetch_max(active, Ordering::SeqCst);
    }
    #[cfg(test)]
    struct ActiveGuard<'a>(&'a Slot);
    #[cfg(test)]
    impl Drop for ActiveGuard<'_> {
        fn drop(&mut self) {
            #[cfg(test)]
            self.0.reconstructions_active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[cfg(test)]
    let _active = ActiveGuard(slot);

    let directory =
        mcp_agent_mail_db::pool::CanonicalSnapshotTempDir::new("agent-mail-read-snapshot-")
            .map_err(AcquireError::failed)?;
    let snapshot_path = directory.path().join("mailbox.sqlite3");
    let stats = if slot.scope.sqlite_path.exists() {
        mcp_agent_mail_db::reconstruct_from_archive_with_live_salvage(
            &snapshot_path,
            &slot.scope.storage_root,
            &slot.scope.sqlite_path,
        )
    } else {
        mcp_agent_mail_db::reconstruct_from_archive(&snapshot_path, &slot.scope.storage_root)
    }
    .map_err(AcquireError::failed)?;
    validate_reconstruction(inventory, &stats)?;

    let probe = private_snapshot_probe(&snapshot_path).map_err(AcquireError::Failed)?;
    let quick_check = probe
        .query_sync("PRAGMA quick_check", &[])
        .map_err(AcquireError::failed)?;
    let healthy = quick_check
        .first()
        .and_then(|row| row.get_named::<String>("quick_check").ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("ok"));
    if !healthy {
        return Err(AcquireError::Failed(
            "reconstructed archive snapshot failed PRAGMA quick_check".to_string(),
        ));
    }
    drop(probe);

    // The reconstructed snapshot is a canonical, sidecar-less file in a
    // process-private directory. The strict query-only pool below opens it
    // through the bound FrankenSQLite opener, which requires the persistent
    // namespace pair, so admit the private family once before handing it to
    // the pool.
    mcp_agent_mail_db::pool::admit_private_database_with_franken(
        &snapshot_path,
        "archive-read snapshot admission",
    )
    .map_err(AcquireError::failed)?;

    let pool = mcp_agent_mail_db::create_query_only_pool(&mcp_agent_mail_db::DbPoolConfig {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&snapshot_path),
        storage_root: Some(slot.scope.storage_root.clone()),
        min_connections: 0,
        max_connections: 4,
        run_migrations: false,
        warmup_connections: 0,
        ..Default::default()
    })
    .map_err(AcquireError::failed)?;
    Ok(Arc::new(SharedSnapshot {
        pool,
        _directory: directory,
    }))
}

struct WorkerPermit;

fn try_reserve_worker(counter: &AtomicUsize, limit: usize) -> bool {
    let mut active = counter.load(Ordering::Acquire);
    loop {
        if active >= limit {
            return false;
        }
        match counter.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => active = observed,
        }
    }
}

impl WorkerPermit {
    fn acquire() -> Option<Self> {
        try_reserve_worker(&WORKERS_ACTIVE, WORKER_LIMIT).then_some(Self)
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        WORKERS_ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

struct BuildOutput {
    generation: Generation,
    cheap: CheapGeneration,
    decision: Decision,
}

fn build_stopped(slot: &Slot, build: &Build) -> Option<AcquireError> {
    if Instant::now() >= build.deadline {
        return Some(AcquireError::TimedOut(format!(
            "archive-read snapshot exceeded its {} second build deadline",
            BUILD_TIMEOUT.as_secs()
        )));
    }
    let invalidated = slot.invalidation_epoch.load(Ordering::Acquire) != build.invalidation_epoch
        || WRITER_EPOCH.load(Ordering::Acquire) != build.writer_epoch
        || WRITERS_ACTIVE.load(Ordering::Acquire) != 0
        || mcp_agent_mail_storage::archive_mutations_active() != 0
        || mcp_agent_mail_storage::archive_mutation_epoch() != build.archive_epoch
        || slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .writers
            != 0;
    invalidated.then(|| {
        AcquireError::Busy(
            "archive-read snapshot build was superseded by a durable write".to_string(),
        )
    })
}

fn run_build(slot: &Slot, database_url: &str, build: &Build) -> Result<BuildOutput, AcquireError> {
    #[cfg(test)]
    {
        let delay = TEST_BUILD_DELAY_MS.load(Ordering::Acquire);
        if delay != 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    for _ in 0..GENERATION_RETRIES {
        if let Some(error) = build_stopped(slot, build) {
            return Err(error);
        }
        let cheap_before = cheap_generation(&slot.scope, build.deadline)?;
        let before = exact_generation(&slot.scope, build.deadline)?;

        let reusable_decision = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready
            .as_ref()
            .filter(|ready| {
                ready.generation == before
                    && ready.invalidation_epoch == build.invalidation_epoch
                    && ready.writer_epoch == build.writer_epoch
            })
            .map(|ready| ready.decision.clone());
        if let Some(decision) = reusable_decision {
            let cheap_after = cheap_generation(&slot.scope, build.deadline)?;
            if cheap_after == cheap_before {
                return Ok(BuildOutput {
                    generation: before,
                    cheap: cheap_after,
                    decision,
                });
            }
            continue;
        }

        let inventory = crate::tool_util::read_archive_inventory(&slot.scope.storage_root);
        if let Some(error) = build_stopped(slot, build) {
            return Err(error);
        }
        let decision = if snapshot_required(&slot.scope, database_url, &inventory)? {
            validate_inventory(&inventory)?;
            Decision::Snapshot(build_snapshot(slot, &inventory)?)
        } else {
            Decision::Live
        };
        tracing::debug!(
            decision = match &decision {
                Decision::Live => "live",
                Decision::Snapshot(_) => "snapshot",
            },
            "archive-read build decision"
        );
        if let Some(error) = build_stopped(slot, build) {
            return Err(error);
        }
        let after = exact_generation(&slot.scope, build.deadline)?;
        let cheap_after = cheap_generation(&slot.scope, build.deadline)?;
        // GH#203: publish on exact-generation (content) stability alone.
        //
        // The snapshot decision above opens the live database, and a
        // FrankenSQLite open can perturb file metadata without changing a byte
        // of content — on macOS/APFS every open bumps the db mtime and
        // checkpoint-truncates the WAL when it can take the lock. A cheap
        // (metadata) comparison spanning those probes therefore never
        // converges on macOS: every build returned Busy, `acquire_if_needed`
        // re-claimed in a tight loop, and every DB-read tool call burned the
        // full dispatch budget.
        //
        // Dropping the cheap half does not weaken the #198 contract. Writes
        // are bracketed by `WriteGuard`, which advances both `WRITER_EPOCH`
        // and the slot invalidation epoch on entry *and* exit, and publication
        // is fenced on those epochs below — so a write that landed and
        // reverted inside the probe window (the case content hashing alone
        // cannot see) is still caught. `cheap_before` remains authoritative on
        // the ready-shortcut arm above, where no probe runs between its two
        // cheap reads. The published baseline is the post-probe `cheap_after`,
        // so the ready fast-path — which opens nothing — stays stable until a
        // durable write actually lands.
        if before == after {
            return Ok(BuildOutput {
                generation: after,
                cheap: cheap_after,
                decision,
            });
        }
    }
    Err(AcquireError::Busy(
        "archive or live SQLite generation moved repeatedly during reconstruction".to_string(),
    ))
}

fn finish_build(slot: &Slot, build: &Build, result: Result<BuildOutput, AcquireError>) {
    mcp_agent_mail_storage::with_archive_snapshot_publication_fence(|| {
        let mut state = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .building
            .as_ref()
            .is_none_or(|active| active.token != build.token)
        {
            drop(state);
            build.completion.finish(Err(AcquireError::Busy(
                "archive-read snapshot build ownership changed before publication".to_string(),
            )));
            return;
        }
        let result = match result {
            Ok(output)
                if Instant::now() < build.deadline
                    && slot.invalidation_epoch.load(Ordering::Acquire)
                        == build.invalidation_epoch
                    && WRITER_EPOCH.load(Ordering::Acquire) == build.writer_epoch
                    && WRITERS_ACTIVE.load(Ordering::Acquire) == 0
                    && mcp_agent_mail_storage::archive_mutations_active() == 0
                    && mcp_agent_mail_storage::archive_mutation_epoch() == build.archive_epoch
                    && state.writers == 0 =>
            {
                state.ready = Some(Ready {
                    generation: output.generation,
                    cheap: output.cheap,
                    decision: output.decision,
                    exact_at: Instant::now(),
                    invalidation_epoch: build.invalidation_epoch,
                    writer_epoch: build.writer_epoch,
                    archive_epoch: build.archive_epoch,
                });
                Ok(())
            }
            Ok(_) => {
                tracing::debug!(
                    deadline_ok = Instant::now() < build.deadline,
                    invalidation_epoch = slot.invalidation_epoch.load(Ordering::Acquire),
                    build_invalidation_epoch = build.invalidation_epoch,
                    writer_epoch = WRITER_EPOCH.load(Ordering::Acquire),
                    build_writer_epoch = build.writer_epoch,
                    global_writers = WRITERS_ACTIVE.load(Ordering::Acquire),
                    archive_mutations = mcp_agent_mail_storage::archive_mutations_active(),
                    archive_epoch = mcp_agent_mail_storage::archive_mutation_epoch(),
                    build_archive_epoch = build.archive_epoch,
                    slot_writers = state.writers,
                    "archive-read publication superseded"
                );
                Err(AcquireError::Busy(
                    "archive-read snapshot build was superseded before publication".to_string(),
                ))
            }
            Err(error) => Err(error),
        };
        state.building = None;
        drop(state);
        // Finishing outside the state lock keeps lock nesting one-level
        // (completion has its own mutex); waiters re-check under the state
        // lock with a bounded wait, so the tiny gap is at most one slice.
        build.completion.finish(result);
    });
    slot.wake.notify_all();
}

fn claim_build(slot: &Slot, deadline: Instant) -> Result<Option<Build>, AcquireError> {
    let mut state = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.building.is_some() {
        return Ok(None);
    }
    if state.writers != 0
        || WRITERS_ACTIVE.load(Ordering::Acquire) != 0
        || mcp_agent_mail_storage::archive_mutations_active() != 0
    {
        tracing::debug!(
            slot_writers = state.writers,
            global_writers = WRITERS_ACTIVE.load(Ordering::Acquire),
            archive_mutations = mcp_agent_mail_storage::archive_mutations_active(),
            "archive-read claim refused: durable writer active"
        );
        return Err(AcquireError::Busy(
            "archive-read snapshot admission is blocked by an active durable writer".to_string(),
        ));
    }
    state.next_token = state.next_token.wrapping_add(1).max(1);
    let build = Build {
        token: state.next_token,
        invalidation_epoch: slot.invalidation_epoch.load(Ordering::Acquire),
        writer_epoch: WRITER_EPOCH.load(Ordering::Acquire),
        archive_epoch: mcp_agent_mail_storage::archive_mutation_epoch(),
        deadline,
        completion: Arc::new(Completion::pending()),
    };
    state.building = Some(build.clone());
    drop(state);
    Ok(Some(build))
}

fn fail_claimed_build(slot: &Slot, build: &Build, error: AcquireError) {
    let mut state = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state
        .building
        .as_ref()
        .is_some_and(|active| active.token == build.token)
    {
        state.building = None;
        build.completion.finish(Err(error));
    }
    drop(state);
    slot.wake.notify_all();
}

struct WorkerBuildGuard {
    slot: Arc<Slot>,
    build: Build,
    finished: bool,
}

impl WorkerBuildGuard {
    const fn new(slot: Arc<Slot>, build: Build) -> Self {
        Self {
            slot,
            build,
            finished: false,
        }
    }
}

impl Drop for WorkerBuildGuard {
    fn drop(&mut self) {
        if !self.finished {
            fail_claimed_build(
                &self.slot,
                &self.build,
                AcquireError::Failed(
                    "archive-read snapshot worker terminated without publishing".to_string(),
                ),
            );
        }
    }
}

fn spawn_build(slot: Arc<Slot>, database_url: String, build: &Build) -> Result<(), AcquireError> {
    let Some(permit) = WorkerPermit::acquire() else {
        let error = AcquireError::Busy(format!(
            "archive-read snapshot workers reached their hard limit of {WORKER_LIMIT}"
        ));
        fail_claimed_build(&slot, build, error.clone());
        return Err(error);
    };
    let worker_build = build.clone();
    let worker_slot = Arc::clone(&slot);
    std::thread::Builder::new()
        .name("am-archive-read".to_string())
        .stack_size(mcp_agent_mail_core::worker_stack_size())
        .spawn(move || {
            let mut guard = WorkerBuildGuard::new(Arc::clone(&worker_slot), worker_build.clone());
            let result = run_build(&worker_slot, &database_url, &worker_build);
            finish_build(&worker_slot, &worker_build, result);
            guard.finished = true;
            drop(permit);
        })
        .map_err(|error| {
            let error = AcquireError::Failed(format!(
                "failed to start archive-read snapshot worker: {error}"
            ));
            fail_claimed_build(&slot, build, error.clone());
            error
        })?;
    Ok(())
}

fn wait_for_build(slot: &Slot, build: &Build, cx: &Cx) -> Result<(), AcquireError> {
    let mut state = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if cx.is_cancel_requested() {
            return Err(AcquireError::Cancelled);
        }
        if let Some(result) = build.completion.result() {
            return result;
        }
        if Instant::now() >= build.deadline {
            return Err(AcquireError::TimedOut(format!(
                "archive-read snapshot wait exceeded its {} second deadline",
                BUILD_TIMEOUT.as_secs()
            )));
        }
        state = slot
            .wake
            .wait_timeout(state, WAIT_SLICE)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

fn wait_retry_slice(slot: &Slot) {
    let state = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(
        slot.wake
            .wait_timeout(state, WAIT_SLICE)
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
}

fn exact_audit_interval() -> Duration {
    std::env::var("MCP_AGENT_MAIL_READ_SNAPSHOT_EXACT_AUDIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(EXACT_AUDIT_INTERVAL, Duration::from_millis)
        .min(Duration::from_secs(30))
}

/// Re-stamp a ready entry's cheap baseline and audit time after its content was
/// confirmed unchanged by an exact re-validation.
///
/// Without this, metadata perturbed by opens would force a full content
/// re-hash on *every* read: the cheap comparison would miss forever because
/// the stored baseline predates the opens. Re-baselining lets the next read
/// take the cheap path again. (Measured on Linux, not just macOS/APFS: a
/// `FrankenSQLite` open bumps the main db's mtime AND ctime and rewrites the
/// `-fsqlite-ns-use` sidecar with zero content change — stat-sampled during
/// the GH#203 livelock at one bump per build cycle.)
///
/// Best-effort: if the cheap generation cannot be computed, or the slot moved
/// on underneath us, we simply leave the entry alone and the next read
/// re-validates. It is never wrong to skip this, only slower.
fn refresh_ready_audit(slot: &Arc<Slot>, deadline: Instant) {
    let Ok(cheap) = cheap_generation(&slot.scope, deadline) else {
        return;
    };
    let mut state = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(ready) = state.ready.as_mut() {
        ready.cheap = cheap;
        ready.exact_at = Instant::now();
    }
}

pub fn acquire_if_needed(
    storage_root: &Path,
    sqlite_path: &Path,
    database_url: &str,
    cx: &Cx,
) -> Result<Option<Arc<SharedSnapshot>>, AcquireError> {
    if cx.is_cancel_requested() {
        return Err(AcquireError::Cancelled);
    }
    let slot = slot_for(scope(storage_root, sqlite_path)?)?;
    let caller_deadline = Instant::now() + BUILD_TIMEOUT;
    loop {
        if cx.is_cancel_requested() {
            return Err(AcquireError::Cancelled);
        }
        if Instant::now() >= caller_deadline {
            return Err(AcquireError::TimedOut(format!(
                "archive-read snapshot acquisition exceeded its {} second deadline",
                BUILD_TIMEOUT.as_secs()
            )));
        }
        let epoch = slot.invalidation_epoch.load(Ordering::Acquire);
        let writer_epoch = WRITER_EPOCH.load(Ordering::Acquire);
        let archive_epoch = mcp_agent_mail_storage::archive_mutation_epoch();
        let (ready, building) = {
            let state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ready = state.ready.as_ref().and_then(|ready| {
                (ready.invalidation_epoch == epoch
                    && ready.writer_epoch == writer_epoch
                    && ready.archive_epoch == archive_epoch
                    && state.writers == 0
                    && WRITERS_ACTIVE.load(Ordering::Acquire) == 0
                    && mcp_agent_mail_storage::archive_mutations_active() == 0)
                    .then(|| {
                        (
                            ready.cheap.clone(),
                            ready.generation.clone(),
                            ready.decision.clone(),
                            ready.exact_at,
                        )
                    })
            });
            (ready, state.building.clone())
        };

        if let Some((expected_cheap, expected_exact, decision, exact_at)) = ready {
            // GH#203 (second half): the cheap generation is metadata-based,
            // and a FrankenSQLite open bumps the live db's mtime/ctime on
            // every open even when no byte of content changes — measured on
            // Linux AND macOS, one bump per build cycle. Reads open the
            // database constantly (run_build itself does), so
            // `observed != expected_cheap` is a *false* invalidation — and
            // past `exact_audit_interval` the cheap path was skipped
            // outright. Both routes fell through to `claim_build`, so every
            // read paid for a full archive reconstruct: a self-invalidation
            // livelock (~30% CPU, a build every ~3 s) that kept
            // `doctor mcp-selftest` at the 45 s timeout on every platform.
            //
            // So a cheap miss no longer implies a rebuild: re-validate against
            // the exact (content) generation, which an open provably does not
            // perturb. One content hash is vastly cheaper than a
            // reconstruction, and it is the same authority `run_build` already
            // publishes on.
            let cheap_fresh = Instant::now().duration_since(exact_at) < exact_audit_interval()
                && cheap_generation(&slot.scope, caller_deadline)? == expected_cheap;

            let content_unchanged = if cheap_fresh {
                true
            } else {
                exact_generation(&slot.scope, caller_deadline)? == expected_exact
            };

            if content_unchanged
                && slot.invalidation_epoch.load(Ordering::Acquire) == epoch
                && WRITER_EPOCH.load(Ordering::Acquire) == writer_epoch
                && WRITERS_ACTIVE.load(Ordering::Acquire) == 0
                && mcp_agent_mail_storage::archive_mutations_active() == 0
                && mcp_agent_mail_storage::archive_mutation_epoch() == archive_epoch
            {
                if !cheap_fresh {
                    // Re-baseline so the next read can take the cheap path
                    // again rather than re-hashing content every time.
                    refresh_ready_audit(&slot, caller_deadline);
                }
                return Ok(decision.snapshot());
            }
        }

        let build = if let Some(build) = building {
            build
        } else {
            let build = match claim_build(&slot, caller_deadline) {
                Ok(Some(build)) => build,
                Ok(None) => continue,
                Err(AcquireError::Busy(_)) => {
                    wait_retry_slice(&slot);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = spawn_build(Arc::clone(&slot), database_url.to_string(), &build) {
                if matches!(error, AcquireError::Busy(_)) {
                    wait_retry_slice(&slot);
                    continue;
                }
                return Err(error);
            }
            build
        };
        match wait_for_build(&slot, &build, cx) {
            Ok(()) => {}
            Err(AcquireError::Busy(_)) => {
                wait_retry_slice(&slot);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut registry = REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(registry.slots.iter().all(|slot| {
        slot.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .building
            .is_none()
    }));
    registry.slots.clear();
    drop(registry);
    TEST_BUILD_DELAY_MS.store(0, Ordering::Release);
}

#[cfg(test)]
pub fn writer_epoch_for_test() -> u64 {
    WRITER_EPOCH.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use std::sync::Barrier;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Proves the configured stack actually carries a frame depth that would
    /// abort on the 2 MiB default, i.e. that `.stack_size()` is really applied.
    #[test]
    fn archive_read_worker_stack_survives_deep_frames() {
        fn burn(depth: usize) -> usize {
            // ~4 KiB of live stack per frame, opaque to the optimizer so the
            // recursion cannot be turned into a loop or elided.
            let mut frame = [0u8; 4096];
            frame[0] = depth.to_le_bytes()[0];
            frame[4095] = depth.to_le_bytes()[0];
            std::hint::black_box(&frame);
            if depth == 0 {
                return 0;
            }
            burn(depth - 1) + usize::from(frame[0])
        }

        // 1024 frames x ~4 KiB is ~4 MiB of stack: comfortably over the 2 MiB
        // spawned-thread default, comfortably under the configured floor.
        let handle = std::thread::Builder::new()
            .name("am-archive-read-stack-test".to_string())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(|| burn(1024))
            .expect("spawn archive-read stack probe");

        handle
            .join()
            .expect("archive-read stack probe must not overflow");
    }

    fn write_archive_fixture(root: &Path) {
        let config = mcp_agent_mail_core::Config {
            storage_root: root.to_path_buf(),
            ..mcp_agent_mail_core::Config::default()
        };
        mcp_agent_mail_storage::ensure_archive_root(&config).expect("initialize archive root");
        let project = root.join("projects").join("single-flight-project");
        let agent = project.join("agents").join("Alice");
        let messages = project.join("messages").join("2026").join("07");
        fs::create_dir_all(&agent).expect("create agent archive");
        fs::create_dir_all(&messages).expect("create message archive");
        fs::write(
            project.join("project.json"),
            r#"{"slug":"single-flight-project","human_key":"/single-flight-project","created_at":0}"#,
        )
        .expect("write project metadata");
        fs::write(
            agent.join("profile.json"),
            r#"{"agent_name":"Alice","program":"test","model":"test","registered_ts":"2026-07-19T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        fs::write(
            messages.join("2026-07-19T00-00-00Z__hello__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[],\"cc\":[],\"bcc\":[],\"subject\":\"hello\",\"body_md\":\"body\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-07-19T00:00:00Z\",\"attachments\":[]}\n---\n\nbody\n",
        )
        .expect("write message");
    }

    fn snapshot_family(path: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        std::iter::once(path.to_path_buf())
            .chain(["-journal", "-wal", "-shm"].into_iter().map(|suffix| {
                let mut value = path.as_os_str().to_os_string();
                value.push(suffix);
                PathBuf::from(value)
            }))
            .map(|path| {
                let bytes = fs::read(&path).ok();
                (path, bytes)
            })
            .collect()
    }

    #[test]
    fn same_size_live_and_wal_mutations_change_exact_generation() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let config = mcp_agent_mail_core::Config {
            storage_root: storage_root.clone(),
            ..mcp_agent_mail_core::Config::default()
        };
        mcp_agent_mail_storage::ensure_archive_root(&config).expect("initialize archive root");
        let sqlite_path = directory.path().join("mailbox.sqlite3");
        fs::write(&sqlite_path, b"first-generation").expect("seed live input");
        let scope = scope(&storage_root, &sqlite_path).expect("scope");
        let before = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("first generation");

        fs::write(&sqlite_path, b"other-generation").expect("same-size mutation");
        let after = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("second generation");

        assert_ne!(before, after);

        fs::write(&sqlite_path, b"first-generation").expect("restore live input");
        let wal_path = PathBuf::from(format!("{}-wal", sqlite_path.display()));
        fs::write(&wal_path, b"wal-generation-a").expect("seed WAL input");
        let wal_before = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("first WAL generation");
        fs::write(&wal_path, b"wal-generation-b").expect("same-size WAL mutation");
        let wal_after = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("second WAL generation");
        assert_ne!(wal_before, wal_after);
    }

    #[test]
    fn worker_reservation_and_deadline_checks_are_hard_bounded() {
        let counter = AtomicUsize::new(0);
        for _ in 0..WORKER_LIMIT {
            assert!(try_reserve_worker(&counter, WORKER_LIMIT));
        }
        assert!(!try_reserve_worker(&counter, WORKER_LIMIT));
        assert_eq!(counter.load(Ordering::Acquire), WORKER_LIMIT);
        assert!(matches!(
            check_deadline(Instant::now(), "test"),
            Err(AcquireError::TimedOut(_))
        ));
    }

    #[test]
    fn registry_refuses_a_seventeenth_live_scope() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let mut held = Vec::with_capacity(REGISTRY_CAPACITY);
        for index in 0..REGISTRY_CAPACITY {
            let storage_root = directory.path().join(format!("archive-{index}"));
            let sqlite_path = directory.path().join(format!("mailbox-{index}.sqlite3"));
            held.push(
                slot_for(scope(&storage_root, &sqlite_path).expect("scope"))
                    .expect("registry slot"),
            );
        }
        let overflow = slot_for(
            scope(
                &directory.path().join("archive-overflow"),
                &directory.path().join("mailbox-overflow.sqlite3"),
            )
            .expect("overflow scope"),
        );
        assert!(matches!(overflow, Err(AcquireError::Busy(_))));
        drop(held);
        reset_for_test();
    }

    #[cfg(unix)]
    #[test]
    fn archive_generation_does_not_walk_project_tree() {
        use std::os::unix::fs::symlink;

        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let config = mcp_agent_mail_core::Config {
            storage_root: storage_root.clone(),
            ..mcp_agent_mail_core::Config::default()
        };
        mcp_agent_mail_storage::ensure_archive_root(&config).expect("initialize archive root");
        let project_root = storage_root.join("projects/project");
        fs::create_dir_all(&project_root).expect("project root");
        let outside = directory.path().join("outside.json");
        fs::write(&outside, b"{}").expect("outside file");
        symlink(&outside, project_root.join("project.json")).expect("project symlink");
        let first = mcp_agent_mail_storage::archive_read_generation(&storage_root)
            .expect("indexed generation must not traverse project artifacts");
        let second = mcp_agent_mail_storage::archive_read_generation(&storage_root)
            .expect("indexed generation must remain stable without a writer");
        assert_eq!(first, second);
    }

    #[test]
    fn durable_db_and_archive_writes_supersede_without_publication() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let sqlite_path = directory.path().join("missing-live.sqlite3");
        write_archive_fixture(&storage_root);
        let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
        let slot = slot_for(scope(&storage_root, &sqlite_path).expect("scope")).expect("slot");

        let db_build = claim_build(&slot, Instant::now() + Duration::from_secs(5))
            .expect("claim db-overlap build")
            .expect("db-overlap owner");
        drop(WriteGuard::begin(&storage_root, Some(&sqlite_path)));
        let db_result = run_build(&slot, &database_url, &db_build);
        assert!(matches!(db_result, Err(AcquireError::Busy(_))));
        finish_build(&slot, &db_build, db_result);
        assert!(
            slot.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .is_none()
        );

        let archive_build = claim_build(&slot, Instant::now() + Duration::from_secs(5))
            .expect("claim archive-overlap build")
            .expect("archive-overlap owner");
        let mut config = mcp_agent_mail_core::Config::default();
        config.storage_root.clone_from(&storage_root);
        mcp_agent_mail_storage::write_op_sync(&mcp_agent_mail_storage::WriteOp::ClearSignal {
            config,
            project_slug: "single-flight-project".to_string(),
            agent_name: "Alice".to_string(),
        })
        .expect("exercise physical archive mutation fence");
        let archive_result = run_build(&slot, &database_url, &archive_build);
        assert!(matches!(archive_result, Err(AcquireError::Busy(_))));
        finish_build(&slot, &archive_build, archive_result);
        assert!(
            slot.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .is_none()
        );
        reset_for_test();
    }

    #[test]
    fn storage_mutation_epoch_and_head_movement_change_exact_generation() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        write_archive_fixture(&storage_root);
        let scope = scope(&storage_root, &directory.path().join("mailbox.sqlite3"))
            .expect("snapshot scope");
        let first = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("first generation");

        let mut config = mcp_agent_mail_core::Config::default();
        config.storage_root.clone_from(&storage_root);
        mcp_agent_mail_storage::write_op_sync(&mcp_agent_mail_storage::WriteOp::ClearSignal {
            config,
            project_slug: "single-flight-project".to_string(),
            agent_name: "Alice".to_string(),
        })
        .expect("storage mutation");
        let moved = exact_generation(&scope, Instant::now() + Duration::from_secs(5))
            .expect("moved generation");
        assert_ne!(moved, first);
    }

    #[test]
    fn writer_guard_advances_epoch_without_health_read_side_effects() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let config = mcp_agent_mail_core::Config {
            storage_root: storage_root.clone(),
            ..mcp_agent_mail_core::Config::default()
        };
        mcp_agent_mail_storage::ensure_archive_root(&config).expect("initialize archive root");
        let sqlite_path = directory.path().join("mailbox.sqlite3");
        let before = WRITER_EPOCH.load(Ordering::Acquire);
        let _ = cheap_generation(
            &scope(&storage_root, &sqlite_path).expect("scope"),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("read-only health-style probe");
        assert_eq!(WRITER_EPOCH.load(Ordering::Acquire), before);
        {
            let _guard = WriteGuard::begin(&storage_root, Some(&sqlite_path));
            assert!(WRITERS_ACTIVE.load(Ordering::Acquire) > 0);
        }
        assert_eq!(WRITER_EPOCH.load(Ordering::Acquire), before + 2);
    }

    #[test]
    fn concurrent_cold_readers_share_one_immutable_query_only_snapshot() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let sqlite_path = directory.path().join("missing-live.sqlite3");
        write_archive_fixture(&storage_root);
        let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
        let scope = scope(&storage_root, &sqlite_path).expect("scope");
        let slot = slot_for(scope).expect("slot");
        TEST_BUILD_DELAY_MS.store(100, Ordering::Release);

        let barrier = Arc::new(Barrier::new(8));
        let mut readers = Vec::new();
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let storage_root = storage_root.clone();
            let sqlite_path = sqlite_path.clone();
            let database_url = database_url.clone();
            readers.push(std::thread::spawn(move || {
                barrier.wait();
                let cx = Cx::for_testing();
                acquire_if_needed(&storage_root, &sqlite_path, &database_url, &cx)
                    .expect("acquire snapshot")
                    .expect("archive must require a snapshot")
            }));
        }
        let snapshots = readers
            .into_iter()
            .map(|reader| reader.join().expect("reader thread"))
            .collect::<Vec<_>>();
        TEST_BUILD_DELAY_MS.store(0, Ordering::Release);

        let pointer = Arc::as_ptr(&snapshots[0]);
        assert!(
            snapshots
                .iter()
                .all(|snapshot| std::ptr::eq(Arc::as_ptr(snapshot), pointer)),
            "all cold readers must share the same published snapshot"
        );
        assert_eq!(slot.reconstructions.load(Ordering::Acquire), 1);
        assert_eq!(slot.reconstructions_max_active.load(Ordering::Acquire), 1);

        let snapshot = &snapshots[0];
        let family_before = snapshot_family(snapshot.path());
        let cx = Cx::for_testing();
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build query runtime");
        let conn = match runtime.block_on(snapshot.pool.acquire(&cx)) {
            asupersync::Outcome::Ok(conn) => conn,
            asupersync::Outcome::Err(error) => {
                panic!("query-only pool acquire failed: {error}")
            }
            asupersync::Outcome::Cancelled(_) => panic!("query-only pool acquire cancelled"),
            asupersync::Outcome::Panicked(_) => panic!("query-only pool acquire panicked"),
        };
        let query_only = conn
            .query_sync("PRAGMA query_only", &[])
            .expect("read query_only pragma")[0]
            .get_named::<i64>("query_only")
            .expect("query_only value");
        assert_eq!(query_only, 1);
        assert!(
            conn.execute_raw("CREATE TABLE forbidden_write(value INTEGER)")
                .is_err(),
            "published snapshot pool must reject writes"
        );
        drop(conn);
        assert_eq!(snapshot_family(snapshot.path()), family_before);
        drop(snapshots);
        reset_for_test();
    }

    #[test]
    fn fetch_inbox_does_not_write_read_receipts_into_archive_snapshot() {
        assert_archive_inbox_preserves_databases(None);
    }

    #[test]
    fn fetch_inbox_from_archive_preserves_unopenable_live_database() {
        assert_archive_inbox_preserves_databases(Some(b"damaged live SQLite fixture"));
    }

    fn assert_archive_inbox_preserves_databases(live_bytes: Option<&[u8]>) {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let sqlite_path = directory.path().join("missing-live.sqlite3");
        if let Some(bytes) = live_bytes {
            fs::write(&sqlite_path, bytes).expect("seed damaged live database");
        }
        let live_family_before = snapshot_family(&sqlite_path);
        write_archive_fixture(&storage_root);
        fs::write(
            storage_root.join(
                "projects/single-flight-project/messages/2026/07/2026-07-19T00-00-00Z__hello__1.md",
            ),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Alice\"],\"subject\":\"hello\",\"importance\":\"normal\",\"ack_required\":true,\"created_ts\":\"2026-07-19T00:00:00Z\"}\n---\n\narchive inbox body\n",
        )
        .expect("write recipient-bearing message");
        let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", storage_root.to_str().expect("storage path")),
            ],
            || {
                mcp_agent_mail_core::Config::reset_cached();
                let cx = Cx::for_testing();
                let snapshot = acquire_if_needed(&storage_root, &sqlite_path, &database_url, &cx)
                    .expect("acquire snapshot")
                    .expect("missing live database requires an archive snapshot");
                let family_before = snapshot_family(snapshot.path());
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                runtime.block_on(async {
                    let ctx = fastmcp::prelude::McpContext::new(cx.clone(), 1);
                    for mark_read in [Some(false), None, Some(true)] {
                        let response = crate::fetch_inbox(
                            &ctx,
                            "/single-flight-project".to_string(),
                            "Alice".to_string(),
                            None,
                            None,
                            Some(10),
                            Some(true),
                            Some(true),
                            None,
                            None,
                            mark_read,
                        )
                        .await
                        .expect("archive inbox remains readable");
                        let messages: serde_json::Value =
                            serde_json::from_str(&response).expect("inbox JSON");
                        assert_eq!(messages.as_array().expect("inbox array").len(), 1);
                        assert_eq!(messages[0]["id"], 1);
                        assert_eq!(messages[0]["from"], "Alice");
                        assert!(
                            messages[0]["body_md"]
                                .as_str()
                                .expect("body")
                                .contains("archive inbox body")
                        );
                        assert!(
                            messages[0]["read_ts"].is_null(),
                            "snapshot reads must not invent durable read receipts: {response}"
                        );
                        assert!(messages[0]["ack_ts"].is_null());
                        assert_eq!(
                            snapshot_family(snapshot.path()),
                            family_before,
                            "snapshot database family changed for mark_read={mark_read:?}"
                        );
                        assert_eq!(
                            snapshot_family(&sqlite_path),
                            live_family_before,
                            "a degraded read must not create or repair the live database"
                        );
                    }
                });
            },
        );
        reset_for_test();
    }

    #[test]
    fn cancelled_waiter_does_not_cancel_single_flight_owner() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let sqlite_path = directory.path().join("missing-live.sqlite3");
        write_archive_fixture(&storage_root);
        let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
        TEST_BUILD_DELAY_MS.store(250, Ordering::Release);

        let owner_root = storage_root.clone();
        let owner_path = sqlite_path.clone();
        let owner_url = database_url.clone();
        let owner = std::thread::spawn(move || {
            let cx = Cx::for_testing();
            acquire_if_needed(&owner_root, &owner_path, &owner_url, &cx)
        });
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while WORKERS_ACTIVE.load(Ordering::Acquire) == 0 && Instant::now() < wait_deadline {
            std::thread::yield_now();
        }
        assert_eq!(WORKERS_ACTIVE.load(Ordering::Acquire), 1);

        let cancelled = Cx::for_testing();
        cancelled.set_cancel_requested(true);
        assert!(matches!(
            acquire_if_needed(&storage_root, &sqlite_path, &database_url, &cancelled),
            Err(AcquireError::Cancelled)
        ));
        assert!(
            owner
                .join()
                .expect("owner thread")
                .expect("owner acquisition")
                .is_some(),
            "the detached owner must finish after another waiter cancels"
        );
        TEST_BUILD_DELAY_MS.store(0, Ordering::Release);
        reset_for_test();
    }

    #[test]
    fn malformed_profile_fails_closed_without_publishing_a_snapshot() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        let directory = tempfile::tempdir().expect("tempdir");
        let storage_root = directory.path().join("archive");
        let sqlite_path = directory.path().join("missing-live.sqlite3");
        write_archive_fixture(&storage_root);
        fs::write(
            storage_root.join("projects/single-flight-project/agents/Alice/profile.json"),
            b"{malformed",
        )
        .expect("corrupt profile");
        let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
        let cx = Cx::for_testing();
        let Err(error) = acquire_if_needed(&storage_root, &sqlite_path, &database_url, &cx) else {
            panic!("malformed profile must fail closed")
        };
        assert!(
            matches!(error, AcquireError::Failed(ref message) if message.contains("skipped")),
            "unexpected failure: {error:?}"
        );
        let slot = slot_for(scope(&storage_root, &sqlite_path).expect("scope")).expect("slot");
        assert!(
            slot.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .is_none()
        );
        reset_for_test();
    }
}
