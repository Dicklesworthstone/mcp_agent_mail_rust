//! Restart-safe hints for terminal reservation reconciliation.
//!
//! One snapshot contains both finite scan positions and every retained retry.
//! It contains no lease payload or publication authority. A restored ID still
//! passes the ordinary live-source, generation and archive ownership checks.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AtomicBool, Config, Cx, DbPool, DeferredRelease, MAX_BACKOFF, MAX_RETRY_IDS,
    ReservationReconcileCursor, ReservationReconcileReport, ScanPosition,
    corruption_circuit_breaker, discovery_query, read_discovery, source_error,
};
#[cfg(test)]
use super::{block_on, outcome};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 128 * 1024;
const OWNED: &str = "reservation reconciliation deferred: checkpoint owned by another worker";

/// The checkpoint owner survives between passes. Keeping its independent flock
/// prevents another process from overwriting retry debt after a failed save.
/// No database reader, writer-admission lease or archive lock survives a pass.
#[derive(Debug, Default)]
pub(super) struct State {
    pub(super) enabled: bool,
    binding: Option<Binding>,
    store: Option<Store>,
    // After an initial filesystem failure, finish this worker's run in memory.
    // Rejoining another owner's checkpoint later could skip its unsaved debt.
    // A fresh worker retries persistence and safely resumes the last snapshot.
    memory_only: bool,
}

impl ReservationReconcileCursor {
    /// Reconcile a bounded pass with restart-safe scan and retry hints.
    ///
    /// Intended for the mailbox maintenance owner. The ordinary one-shot API
    /// remains in-memory. Hints live beside SQLite, outside the archive tree,
    /// and are bound to the database inode, logical generation and archive root.
    /// Competing checkpoint owners yield without scanning. Unavailable hint
    /// storage falls back to in-memory repair for this cursor's lifetime.
    pub fn reconcile_checkpointed(
        &mut self,
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        stop: &AtomicBool,
    ) -> Result<ReservationReconcileReport, String> {
        self.retries.checkpoint.enabled = true;
        super::run(cx, pool, config, self, stop)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    database: PathBuf,
    archive: PathBuf,
    file_identity: String,
    generation: Option<String>,
}

impl Binding {
    fn capture(cx: &Cx, pool: &DbPool, config: &Config) -> Result<Self, String> {
        let generation = read_discovery(cx, pool, |conn| {
            // Bound the projection before materializing an untrusted generation.
            let rows = discovery_query(
                conn,
                "SELECT CASE WHEN length(CAST(generation_id AS BLOB)) BETWEEN 1 AND 128 \
                 THEN generation_id ELSE NULL END AS generation_id \
                 FROM db_identity WHERE singleton = 0",
                &[],
            )?;
            match rows.as_slice() {
                [] => Ok(None),
                [row] => Ok(Some(
                    row.get_named::<Option<String>>("generation_id")
                        .map_err(source_error)?
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 128
                                && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                        .ok_or("reservation checkpoint source generation is invalid")?,
                )),
                _ => Err("reservation checkpoint source generation is ambiguous".into()),
            }
        })?;
        Self::at(
            Path::new(pool.sqlite_path()),
            &config.storage_root,
            generation,
        )
        .map_err(|error| format!("reservation checkpoint source identity failed: {error}"))
    }

    fn at(database: &Path, archive: &Path, generation: Option<String>) -> io::Result<Self> {
        let database = std::fs::canonicalize(database)?;
        let archive = std::fs::canonicalize(archive)?;
        #[cfg(windows)]
        let source = mcp_agent_mail_core::disk::open_regular_file_no_follow(&database)?;
        #[cfg(windows)]
        let metadata = source.metadata()?;
        #[cfg(not(windows))]
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
        let file_identity = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}:{created:?}", metadata.dev(), metadata.ino())
        };
        #[cfg(windows)]
        let file_identity = {
            let (volume, index) = mcp_agent_mail_core::disk::windows_file_identity(&source)?;
            let created =
                created.ok_or_else(|| invalid("checkpoint source has no stable file identity"))?;
            // NTFS can preserve creation time when a different file replaces
            // this path. Bind to the opened file's identity as well.
            format!("{volume}:{index}:{created:?}")
        };
        #[cfg(not(any(unix, windows)))]
        let file_identity = format!(
            "{:?}",
            created.ok_or_else(|| invalid("checkpoint source has no stable file identity"))?
        );
        Ok(Self {
            database,
            archive,
            file_identity,
            generation,
        })
    }

    fn path(&self) -> io::Result<PathBuf> {
        let parent = self
            .database
            .parent()
            .ok_or_else(|| invalid("database has no parent"))?;
        let mut digest = Sha256::new();
        digest.update(self.database.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(self.archive.as_os_str().as_encoded_bytes());
        Ok(parent.join(format!(
            ".am-reservation-reconcile-{}.json",
            hex::encode(digest.finalize())
        )))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Position {
    after: i64,
    ceiling: Option<i64>,
}

impl Position {
    fn capture(position: &ScanPosition) -> Self {
        Self {
            after: position.after,
            ceiling: position.ceiling,
        }
    }

    fn valid(&self) -> bool {
        self.after >= 0 && self.ceiling.is_none_or(|ceiling| ceiling >= self.after)
    }

    fn restore(self) -> ScanPosition {
        ScanPosition {
            after: self.after,
            ceiling: self.ceiling,
        }
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
    priority: Position,
    history: Position,
    retries: Vec<Retry>,
}

impl Snapshot {
    fn capture(
        binding: Binding,
        cursor: &ReservationReconcileCursor,
        now: Instant,
        wall: i64,
    ) -> Self {
        Self {
            version: VERSION,
            binding,
            saved_at_us: wall,
            priority: Position::capture(&cursor.priority),
            history: Position::capture(&cursor.history),
            retries: cursor
                .retries
                .pending
                .iter()
                .map(|retry| Retry {
                    id: retry.id,
                    attempts: retry.attempts,
                    wait_us: u64::try_from(
                        retry
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
                "checkpoint source, archive, generation or version changed",
            ));
        }
        if self.saved_at_us <= 0
            || !self.priority.valid()
            || !self.history.valid()
            || self.retries.len() > MAX_RETRY_IDS
        {
            return Err(invalid("checkpoint scheduling bounds are invalid"));
        }
        let mut ids = HashSet::new();
        let max_wait = u64::try_from(MAX_BACKOFF.as_micros()).unwrap_or(u64::MAX);
        for retry in &self.retries {
            if retry.id <= 0 || retry.wait_us > max_wait || !ids.insert(retry.id) {
                return Err(invalid("checkpoint retry IDs or cooldowns are invalid"));
            }
        }
        Ok(())
    }

    fn restore(self, cursor: &mut ReservationReconcileCursor, now: Instant, wall: i64) {
        // Instant belongs to one process. A backwards wall clock can restore
        // at most the original bounded delay, never an indefinite cooldown.
        let elapsed = u64::try_from(wall.saturating_sub(self.saved_at_us)).unwrap_or(0);
        cursor.priority = self.priority.restore();
        cursor.history = self.history.restore();
        cursor.retries.pending = self
            .retries
            .into_iter()
            .map(|retry| DeferredRelease {
                id: retry.id,
                attempts: retry.attempts,
                ready_at: now + Duration::from_micros(retry.wait_us.saturating_sub(elapsed)),
            })
            .collect();
        // Do not restore process-local pool identity or checkpoint ownership.
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_sidecar(path: &Path) -> io::Result<()> {
    if crate::path_existing_prefix_has_symlink(path).map_err(|error| invalid(&error.to_string()))? {
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

fn contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
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
        let lock_path = path.with_extension("json.lock");
        check_sidecar(&path)?;
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
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let lock = options.open(&lock_path)?;
        let store = Self {
            binding,
            path,
            lock_path,
            lock,
        };
        store.check_owner()?;
        FileExt::try_lock_exclusive(&store.lock)?;
        Ok(store)
    }

    fn check_owner(&self) -> io::Result<()> {
        check_sidecar(&self.lock_path)?;
        let held = self.lock.metadata()?;
        let named = std::fs::symlink_metadata(&self.lock_path)?;
        if !held.is_file() || !named.is_file() {
            return Err(invalid("checkpoint owner is not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if held.nlink() != 1 || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
                return Err(invalid("checkpoint owner inode changed"));
            }
        }
        #[cfg(windows)]
        {
            let named_file =
                mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.lock_path)?;
            if mcp_agent_mail_core::disk::windows_file_identity(&self.lock)?
                != mcp_agent_mail_core::disk::windows_file_identity(&named_file)?
            {
                return Err(invalid("checkpoint owner file identity changed"));
            }
        }
        Ok(())
    }

    fn load(&self) -> io::Result<Option<Snapshot>> {
        self.check_owner()?;
        check_sidecar(&self.path)?;
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &self.path, MAX_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(|error| invalid(&error.to_string()))?;
        snapshot.validate(&self.binding)?;
        Ok(Some(snapshot))
    }

    #[cfg(test)]
    fn save(&self, cursor: &ReservationReconcileCursor) -> io::Result<()> {
        self.save_with(cursor, || Ok(()))
    }

    /// Stage durable bytes before reacquiring source authority. The returned
    /// guard covers only final ownership checks and atomic publication, not
    /// serialization, staged-file fsync or directory fsync.
    fn save_with<T>(
        &self,
        cursor: &ReservationReconcileCursor,
        before_publish: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<()> {
        self.check_owner()?;
        let current = Binding::at(
            &self.binding.database,
            &self.binding.archive,
            self.binding.generation.clone(),
        )?;
        if current != self.binding {
            return Err(invalid(
                "checkpoint database was replaced during reconciliation",
            ));
        }
        check_sidecar(&self.path)?;
        let snapshot = Snapshot::capture(
            current,
            cursor,
            Instant::now(),
            mcp_agent_mail_db::now_micros(),
        );
        snapshot.validate(&self.binding)?;
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| invalid(&error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(invalid("checkpoint exceeds its byte budget"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("checkpoint has no parent"))?;
        let mut candidate = tempfile::Builder::new()
            .prefix(".am-reservation-reconcile-")
            .tempfile_in(parent)?;
        candidate.write_all(&bytes)?;
        candidate.as_file().sync_all()?;
        #[cfg(test)]
        tests::observe_boundary(tests::Boundary::Staged);
        let admission = before_publish()?;
        // Slow staging cannot preserve ownership of a swapped checkpoint
        // path or attach these hints to a replaced source file.
        self.check_owner()?;
        if Binding::at(
            &self.binding.database,
            &self.binding.archive,
            self.binding.generation.clone(),
        )? != self.binding
        {
            return Err(invalid(
                "checkpoint database was replaced before publication",
            ));
        }
        check_sidecar(&self.path)?;
        #[cfg(test)]
        tests::observe_boundary(tests::Boundary::Admitted);
        candidate.persist(&self.path).map_err(|error| error.error)?;
        drop(admission);
        #[cfg(test)]
        tests::observe_boundary(tests::Boundary::Published);
        // A later promotion may now proceed. The published hints still name
        // the old binding, so their durability cannot grant authority to the
        // new generation. Retain our independent checkpoint flock throughout.
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn warn_unavailable(error: &io::Error) {
    tracing::warn!(
        target: "maintenance", event = "reservation_archive_checkpoint_unavailable",
        reason = %error,
        "reservation repair continues in memory; restart progress is not durable"
    );
}

/// Ordinary writable-source admission has completed, but no database activity
/// or connection spans this wrapper. Each source scope admits independently.
pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReservationReconcileReport, String> {
    if !cursor.retries.checkpoint.enabled {
        return super::run_batch(cx, pool, config, cursor, stop, None);
    }
    let mut state = std::mem::take(&mut cursor.retries.checkpoint);
    let result = run_with_state(cx, pool, config, cursor, stop, &mut state);
    cursor.retries.checkpoint = state;
    result
}

fn run_with_state(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
    state: &mut State,
) -> Result<ReservationReconcileReport, String> {
    let binding = {
        let _admission = super::admit_source(pool, config, Some(&cursor.source_identity))?;
        Binding::capture(cx, pool, config)?
    };
    if state
        .binding
        .as_ref()
        .is_some_and(|previous| previous != &binding)
    {
        // A same-pool logical-generation change also invalidates every hint.
        // Never carry an old generation's retained IDs into a new identity.
        cursor.priority = ScanPosition::default();
        cursor.history = ScanPosition::default();
        cursor.retries.pending.clear();
        state.store = None;
        state.memory_only = false;
    }
    state.binding = Some(binding.clone());
    let mut restored = false;
    if state.store.is_none() && !state.memory_only {
        match Store::open(binding.clone()) {
            Ok(store) => {
                match store.load() {
                    Ok(Some(snapshot)) => {
                        #[cfg(test)]
                        tests::observe_boundary(tests::Boundary::Loaded);
                        // Checkpoint I/O deliberately ran without promotion
                        // exclusion. Revalidate before applying any old hint.
                        let _admission =
                            super::admit_source(pool, config, Some(&cursor.source_identity))?;
                        if Binding::capture(cx, pool, config)? != binding {
                            cursor.source_identity.clear();
                            return Err(
                                "reservation checkpoint source changed before restore; rescan required"
                                    .into(),
                            );
                        }
                        snapshot.restore(cursor, Instant::now(), mcp_agent_mail_db::now_micros());
                        restored = true;
                    }
                    Ok(None) => {}
                    Err(error) => warn_unavailable(&error),
                }
                state.store = Some(store);
            }
            Err(error) if contended(&error) => return Err(OWNED.into()),
            Err(error) => {
                warn_unavailable(&error);
                state.memory_only = true;
            }
        }
    }
    let result = super::run_batch(
        cx,
        pool,
        config,
        cursor,
        stop,
        Some(binding.generation.as_deref()),
    );
    let saved = if corruption_circuit_breaker().is_tripped() {
        false
    } else if let Some(store) = &state.store {
        match save_current(cx, pool, config, cursor, stop, store) {
            Ok(()) => true,
            Err(error) => {
                warn_unavailable(&error);
                false
            }
        }
    } else {
        false
    };
    tracing::debug!(
        target: "maintenance", event = "reservation_archive_checkpoint",
        restored, saved, retry_len = cursor.retries.pending.len(),
        "reservation scan positions and retry debt checkpointed together"
    );
    result
}

/// Stage without database activity, then bind the short rename window to a
/// fresh live-source observation. Failed admission preserves the old snapshot
/// and all in-memory debt; the independent flock prevents a competing writer
/// from replacing it while this cursor retries.
fn save_current(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
    store: &Store,
) -> io::Result<()> {
    let check_live = || {
        if stop.load(Ordering::Acquire)
            || cx.checkpoint().is_err()
            || corruption_circuit_breaker().is_tripped()
        {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "reservation checkpoint publication interrupted",
            ))
        } else {
            Ok(())
        }
    };
    check_live()?;
    let source_identity = cursor.source_identity.clone();
    let mut changed = false;
    let result = store.save_with(cursor, || {
        check_live()?;
        let admission =
            super::admit_source(pool, config, Some(&source_identity)).map_err(io::Error::other)?;
        if Binding::capture(cx, pool, config).map_err(io::Error::other)? != store.binding {
            changed = true;
            return Err(invalid(
                "reservation checkpoint source changed during reconciliation",
            ));
        }
        check_live()?;
        Ok(admission)
    });
    if changed {
        // A subsequent pass must discard this binding's in-memory positions
        // as well as refusing its staged durable snapshot.
        cursor.source_identity.clear();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::super::tests as release_tests;
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Boundary {
        Loaded,
        Staged,
        Admitted,
        Published,
    }

    type BoundaryObserver = Box<dyn FnMut(Boundary)>;

    std::thread_local! {
        static BOUNDARY_OBSERVER: std::cell::RefCell<Option<BoundaryObserver>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn observe_boundary(boundary: Boundary) {
        BOUNDARY_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow_mut().as_mut() {
                observer(boundary);
            }
        });
    }

    struct ObserveBoundary;

    impl ObserveBoundary {
        fn new(observer: impl FnMut(Boundary) + 'static) -> Self {
            BOUNDARY_OBSERVER.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(Box::new(observer));
            });
            Self
        }
    }

    impl Drop for ObserveBoundary {
        fn drop(&mut self) {
            BOUNDARY_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    #[test]
    fn checkpoint_staging_and_directory_sync_do_not_retain_database_activity() {
        use mcp_agent_mail_db::write_barrier::{
            active_writer_count, try_acquire_promotion_barrier_if_idle,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        super::super::super::tests::fixture(|cx, pool, config| {
            let binding = Binding::capture(cx, pool, config).unwrap();
            let observations = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&observations);
            let observer_binding = binding.clone();
            let observer = ObserveBoundary::new(move |stage| {
                let writers = active_writer_count();
                let promotion = try_acquire_promotion_barrier_if_idle();
                let checkpoint_owned =
                    Store::open(observer_binding.clone()).is_err_and(|error| contended(&error));
                observed
                    .borrow_mut()
                    .push((stage, writers, promotion.is_some(), checkpoint_owned));
            });
            let mut cursor = ReservationReconcileCursor::default();
            let result = cursor.reconcile_checkpointed(cx, pool, config, &AtomicBool::new(false));
            drop(observer);
            assert_eq!(result.unwrap().repaired, 1);
            assert_eq!(
                *observations.borrow(),
                [
                    (Boundary::Staged, 0, true, true),
                    (Boundary::Admitted, 1, false, true),
                    (Boundary::Published, 0, true, true),
                ]
            );
            let bytes = std::fs::read(binding.path().unwrap()).unwrap();
            let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
            snapshot.validate(&binding).unwrap();
            assert!(snapshot.retries.is_empty());
            assert_eq!(active_writer_count(), 0);
        });
    }

    fn persisted_retry(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
    ) -> (ReservationReconcileCursor, Binding, Vec<u8>) {
        let archive = crate::ensure_archive(config, "project").unwrap();
        let process = crate::archive_process_lock(&archive).unwrap();
        let _held = process.lock().unwrap();
        let mut cursor = ReservationReconcileCursor::default();
        let report = cursor
            .reconcile_checkpointed(cx, pool, config, &AtomicBool::new(false))
            .unwrap();
        assert_eq!((report.deferred_transient, report.retry_len), (1, 1));
        let binding = Binding::capture(cx, pool, config).unwrap();
        let bytes = std::fs::read(binding.path().unwrap()).unwrap();
        (cursor, binding, bytes)
    }

    fn save_pending(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        cursor: &mut ReservationReconcileCursor,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        let state = std::mem::take(&mut cursor.retries.checkpoint);
        let result = save_current(
            cx,
            pool,
            config,
            cursor,
            stop,
            state.store.as_ref().unwrap(),
        );
        cursor.retries.checkpoint = state;
        result
    }

    #[test]
    fn discovery_failures_preserve_checkpoint_debt_and_eventually_publish_fresh_release() {
        if release_tests::isolated_admission_test() {
            return;
        }
        // Real checkpointed entrypoints: writable admission, binding capture,
        // priority page after its aggregate, and history page after its aggregate.
        for failing_query in [1, 2, 5, 8] {
            for panic in [false, true] {
                release_tests::fixture(|cx, pool, config| {
                    let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                    let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
                    conn.execute_raw(&format!(
                        "UPDATE file_reservations SET expires_ts = {expires} WHERE id = 401"
                    ))
                    .unwrap();
                    drop(conn);
                    let source = super::super::super::read_source(cx, pool, 401)
                        .unwrap()
                        .unwrap();
                    let (path, original) =
                        release_tests::seed_artifact(config, &source, "aabb", false);
                    let (mut cursor, binding, _) = persisted_retry(cx, pool, config);
                    cursor.priority = ScanPosition {
                        after: 17,
                        ceiling: None,
                    };
                    cursor.history = ScanPosition {
                        after: 23,
                        ceiling: None,
                    };
                    cursor.retries.pending[0].ready_at = Instant::now() + MAX_BACKOFF;
                    let stop = AtomicBool::new(false);
                    save_pending(cx, pool, config, &mut cursor, &stop).unwrap();
                    let previous = std::fs::read(binding.path().unwrap()).unwrap();
                    let debt = (
                        cursor.retries.pending[0].id,
                        cursor.retries.pending[0].attempts,
                        cursor.retries.pending[0].ready_at,
                    );

                    let fault = release_tests::DiscoveryReadFault::new(failing_query, panic);
                    let error = cursor
                        .reconcile_checkpointed(cx, pool, config, &stop)
                        .unwrap_err();
                    fault.assert_triggered();
                    drop(fault);
                    assert!(!error.contains("private-reservation-discovery-panic-payload"));
                    assert!(error.contains(if panic {
                        "panicked"
                    } else {
                        "source query failed"
                    }));
                    assert_eq!(std::fs::read(&path).unwrap(), original);
                    assert_eq!(cursor.retries.pending.len(), 1);
                    assert_eq!(
                        (
                            cursor.retries.pending[0].id,
                            cursor.retries.pending[0].attempts,
                            cursor.retries.pending[0].ready_at,
                        ),
                        debt,
                    );
                    // A failed selection keeps its old hints. An earlier
                    // completed priority page is still legitimate progress.
                    assert_eq!(
                        (cursor.priority.after, cursor.priority.ceiling),
                        (if failing_query == 8 { 401 } else { 17 }, None),
                    );
                    assert_eq!((cursor.history.after, cursor.history.ceiling), (23, None));
                    assert!(Store::open(binding.clone()).is_err_and(|error| contended(&error)));
                    let saved = std::fs::read(binding.path().unwrap()).unwrap();
                    if failing_query <= 2 {
                        assert_eq!(saved, previous);
                    }
                    let snapshot: Snapshot = serde_json::from_slice(&saved).unwrap();
                    snapshot.validate(&binding).unwrap();
                    assert_eq!(snapshot.retries.len(), 1);
                    assert_eq!(snapshot.retries[0].id, 401);
                    assert_eq!(snapshot.history.after, 23);
                    assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);

                    let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                    assert_eq!(
                        conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                            .get_as::<i64>(0)
                            .unwrap(),
                        0,
                        "the sole failed reader was returned to the pool",
                    );
                    let rows = conn
                        .query_sync(
                            "SELECT fr.released_ts, rr.released_ts FROM file_reservations fr \
                         JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
                         WHERE fr.id = 401",
                            &[],
                        )
                        .unwrap();
                    assert_eq!(rows[0].get_as::<Option<i64>>(0).unwrap(), None);
                    assert_eq!(rows[0].get_as::<i64>(1).unwrap(), 5_000_000);
                    conn.execute_raw(
                        "UPDATE file_reservations SET reason = 'fresh source after failure' WHERE id = 401"
                    ).unwrap();
                    conn.execute_raw(
                        "UPDATE file_reservation_releases SET released_ts = 6000000 WHERE reservation_id = 401"
                    ).unwrap();
                    drop(conn);
                    cursor.retries.pending[0].ready_at = Instant::now();
                    let report = cursor
                        .reconcile_checkpointed(cx, pool, config, &stop)
                        .unwrap();
                    assert_eq!((report.repaired, report.retry_len), (1, 0), "{report:?}");
                    let bytes = std::fs::read(&path).unwrap();
                    let artifact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(
                        super::super::super::timestamp(&artifact["released_ts"]),
                        Some(6_000_000),
                    );
                    assert_eq!(artifact["reason"], "fresh source after failure");
                    assert_eq!(artifact["operator_note"], "preserve this note");
                    assert_eq!(artifact["db_generation"], "aabb");
                    let repo = git2::Repository::open(&config.storage_root).unwrap();
                    let tree = repo.head().unwrap().peel_to_tree().unwrap();
                    let entry = tree
                        .get_path(Path::new(
                            "projects/project/file_reservations/id-401-gaabb.json",
                        ))
                        .unwrap();
                    assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
                    assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
                });
            }
        }
    }

    #[test]
    fn checkpoint_save_source_failures_preserve_owned_snapshot_until_retry() {
        if release_tests::isolated_admission_test() {
            return;
        }
        for panic in [false, true] {
            release_tests::fixture(|cx, pool, config| {
                let (mut cursor, binding, previous) = persisted_retry(cx, pool, config);
                cursor.retries.pending[0].attempts = 2;
                cursor.retries.pending[0].ready_at = Instant::now() + MAX_BACKOFF;
                let stop = AtomicBool::new(false);
                let fault = release_tests::DiscoveryReadFault::new(1, panic);
                let error = save_pending(cx, pool, config, &mut cursor, &stop).unwrap_err();
                fault.assert_triggered();
                drop(fault);
                assert!(
                    !error
                        .to_string()
                        .contains("private-reservation-discovery-panic-payload")
                );
                assert_eq!(std::fs::read(binding.path().unwrap()).unwrap(), previous);
                assert_eq!(cursor.retries.pending.len(), 1);
                assert_eq!(cursor.retries.pending[0].attempts, 2);
                assert!(Store::open(binding.clone()).is_err_and(|error| contended(&error)));
                assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert_eq!(
                    conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                        .get_as::<i64>(0)
                        .unwrap(),
                    0,
                );
                drop(conn);
                save_pending(cx, pool, config, &mut cursor, &stop).unwrap();
                let snapshot: Snapshot =
                    serde_json::from_slice(&std::fs::read(binding.path().unwrap()).unwrap())
                        .unwrap();
                snapshot.validate(&binding).unwrap();
                assert_eq!(snapshot.retries[0].attempts, 2);
                cursor.retries.pending[0].ready_at = Instant::now();
                let report = cursor
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap();
                assert_eq!((report.repaired, report.retry_len), (1, 0));
            });
        }
    }

    #[test]
    fn metadata_refusals_keep_healthy_readers_and_release_debt() {
        release_tests::fixture(|cx, pool, config| {
            let (mut cursor, binding, previous) = persisted_retry(cx, pool, config);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            drop(conn);
            let stop = AtomicBool::new(false);
            let error = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap_err();
            assert!(error.contains("query-only snapshots"));
            assert_eq!(std::fs::read(binding.path().unwrap()).unwrap(), previous);
            assert_eq!(cursor.retries.pending.len(), 1);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                1
            );
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
            conn.execute_raw(
                "UPDATE db_identity SET generation_id = 'not-hex' WHERE singleton = 0",
            )
            .unwrap();
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            drop(conn);
            // A successful metadata read of an invalid generation is also a
            // refusal, without retiring an otherwise healthy source reader.
            let error = Binding::capture(cx, pool, config).unwrap_err();
            assert!(error.contains("generation is invalid"));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                1
            );
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
            conn.execute_raw("UPDATE db_identity SET generation_id = 'aabb' WHERE singleton = 0")
                .unwrap();
            drop(conn);
            assert!(Store::open(binding).is_err_and(|error| contended(&error)));
            let report = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!((report.repaired, report.retry_len), (1, 0));
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn promotion_during_staging_preserves_checkpoint_and_retry_until_resume() {
        use mcp_agent_mail_db::write_barrier::try_acquire_promotion_barrier_if_idle;
        use std::cell::RefCell;
        use std::rc::Rc;

        super::super::super::tests::fixture(|cx, pool, config| {
            let (mut cursor, binding, previous) = persisted_retry(cx, pool, config);
            let stop = AtomicBool::new(false);
            let held = Rc::new(RefCell::new(None));
            let observer_held = Rc::clone(&held);
            let stages = Rc::new(RefCell::new(Vec::new()));
            let observer_stages = Rc::clone(&stages);
            let observer = ObserveBoundary::new(move |stage| {
                observer_stages.borrow_mut().push(stage);
                if stage == Boundary::Staged {
                    *observer_held.borrow_mut() = Some(
                        try_acquire_promotion_barrier_if_idle()
                            .expect("staged checkpoint retained promotion exclusion"),
                    );
                }
            });
            let error = save_pending(cx, pool, config, &mut cursor, &stop).unwrap_err();
            drop(observer);
            let promotion_admitted = held.borrow().is_some();
            drop(held.borrow_mut().take());
            assert!(promotion_admitted);
            assert!(error.to_string().contains("admission contention"));
            assert_eq!(*stages.borrow(), [Boundary::Staged]);
            assert_eq!(std::fs::read(binding.path().unwrap()).unwrap(), previous);
            assert_eq!(cursor.retries.pending.len(), 1);
            assert_eq!(cursor.retries.pending[0].id, 401);
            assert_eq!(cursor.retries.pending[0].attempts, 0);
            assert!(contended(&Store::open(binding.clone()).unwrap_err()));

            save_pending(cx, pool, config, &mut cursor, &stop).unwrap();
            let snapshot: Snapshot =
                serde_json::from_slice(&std::fs::read(binding.path().unwrap()).unwrap()).unwrap();
            snapshot.validate(&binding).unwrap();
            assert_eq!(snapshot.retries[0].id, 401);
            let report = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!((report.repaired, report.retry_len), (1, 0));
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    fn exercise_generation_change(boundary: Boundary) {
        use mcp_agent_mail_db::write_barrier::try_acquire_promotion_barrier_if_idle;
        use std::cell::Cell;
        use std::rc::Rc;

        super::super::super::tests::fixture(|cx, pool, config| {
            let (mut cursor, binding, previous) = persisted_retry(cx, pool, config);
            if boundary == Boundary::Loaded {
                // Drop the real checkpoint flock as an exiting worker would.
                cursor = ReservationReconcileCursor::default();
            }
            let archive = crate::ensure_archive(config, "project").unwrap();
            let process = crate::archive_process_lock(&archive).unwrap();
            let held = process.lock().unwrap();
            let changed = Rc::new(Cell::new(false));
            let observer_changed = Rc::clone(&changed);
            let observer_pool = pool.clone();
            let observer_cx = cx.clone();
            let observer = ObserveBoundary::new(move |stage| {
                if stage != boundary || observer_changed.replace(true) {
                    return;
                }
                let _promotion = try_acquire_promotion_barrier_if_idle()
                    .expect("checkpoint I/O retained promotion exclusion");
                let conn = outcome(block_on(observer_pool.acquire(&observer_cx))).unwrap();
                conn.execute_raw("UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0")
                    .unwrap();
            });
            let stop = AtomicBool::new(false);
            let result = cursor.reconcile_checkpointed(cx, pool, config, &stop);
            drop(observer);
            drop(held);
            assert!(changed.get());
            if boundary == Boundary::Loaded {
                assert!(
                    result
                        .unwrap_err()
                        .contains("source changed before restore")
                );
                assert_eq!(cursor.history.after, 0);
                assert!(
                    cursor.retries.pending.is_empty(),
                    "foreign hints were restored"
                );
            } else {
                assert_eq!(result.unwrap().deferred_transient, 1);
                assert_eq!(cursor.retries.pending.len(), 1);
                assert_eq!(
                    (
                        cursor.retries.pending[0].id,
                        cursor.retries.pending[0].attempts
                    ),
                    (401, 1)
                );
            }
            assert!(cursor.source_identity.is_empty());
            assert_eq!(std::fs::read(binding.path().unwrap()).unwrap(), previous);
            for generation in ["aabb", "ccdd"] {
                assert!(
                    !config
                        .storage_root
                        .join(format!(
                            "projects/project/file_reservations/id-401-g{generation}.json"
                        ))
                        .exists()
                );
            }
            let current = Binding::capture(cx, pool, config).unwrap();
            assert_eq!(current.file_identity, binding.file_identity);
            assert_eq!(current.generation.as_deref(), Some("ccdd"));
            let resumed = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!((resumed.repaired, resumed.retry_len), (1, 0));
            assert!(
                config
                    .storage_root
                    .join("projects/project/file_reservations/id-401-gccdd.json")
                    .is_file()
            );
            let snapshot: Snapshot =
                serde_json::from_slice(&std::fs::read(current.path().unwrap()).unwrap()).unwrap();
            snapshot.validate(&current).unwrap();
            assert!(snapshot.retries.is_empty());
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn loaded_checkpoint_is_revalidated_before_restoring_hints() {
        exercise_generation_change(Boundary::Loaded);
    }

    #[test]
    fn staged_checkpoint_cannot_publish_after_same_inode_generation_change() {
        exercise_generation_change(Boundary::Staged);
    }

    #[test]
    fn cancellation_during_staging_preserves_previous_checkpoint_and_debt() {
        use std::sync::Arc;

        super::super::super::tests::fixture(|cx, pool, config| {
            let (mut cursor, binding, previous) = persisted_retry(cx, pool, config);
            let stop = Arc::new(AtomicBool::new(false));
            let observer_stop = Arc::clone(&stop);
            let observer = ObserveBoundary::new(move |stage| {
                assert_eq!(stage, Boundary::Staged);
                observer_stop.store(true, Ordering::Release);
            });
            let result = save_pending(cx, pool, config, &mut cursor, &stop);
            drop(observer);
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
            assert_eq!(std::fs::read(binding.path().unwrap()).unwrap(), previous);
            assert_eq!(cursor.retries.pending.len(), 1);
            assert_eq!(
                (
                    cursor.retries.pending[0].id,
                    cursor.retries.pending[0].attempts
                ),
                (401, 0)
            );
            assert!(contended(&Store::open(binding).unwrap_err()));
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
            stop.store(false, Ordering::Release);
            save_pending(cx, pool, config, &mut cursor, &stop).unwrap();
        });
    }

    fn binding_fixture() -> (tempfile::TempDir, Binding) {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("mail.sqlite3");
        let archive = temp.path().join("archive");
        std::fs::write(&database, b"source bytes").unwrap();
        std::fs::create_dir(&archive).unwrap();
        let binding = Binding::at(&database, &archive, Some("aabb".into())).unwrap();
        (temp, binding)
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

    fn debt(now: Instant) -> ReservationReconcileCursor {
        let mut cursor = ReservationReconcileCursor {
            source_identity: "current-process".into(),
            priority: ScanPosition {
                after: 90,
                ceiling: Some(100),
            },
            history: ScanPosition {
                after: 40,
                ceiling: Some(100),
            },
            ..Default::default()
        };
        cursor.retries.pending.push_back(DeferredRelease {
            id: 7,
            attempts: 0,
            ready_at: now,
        });
        cursor.retries.pending.push_back(DeferredRelease {
            id: 80,
            attempts: 6,
            ready_at: now + Duration::from_secs(30),
        });
        cursor
    }

    #[test]
    fn restores_both_scan_positions_and_retry_cooldowns_without_pool_authority() {
        let (_temp, binding) = binding_fixture();
        let now = Instant::now();
        for (wall, wait) in [(11_000_000, 20), (100, 30), (91_000_000, 0)] {
            let snapshot = Snapshot::capture(binding.clone(), &debt(now), now, 1_000_000);
            let bytes = serde_json::to_vec(&snapshot).unwrap();
            let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
            snapshot.validate(&binding).unwrap();
            let mut cursor = ReservationReconcileCursor {
                source_identity: "new-process".into(),
                ..Default::default()
            };
            snapshot.restore(&mut cursor, now, wall);
            assert_eq!(
                (cursor.priority.after, cursor.priority.ceiling),
                (90, Some(100))
            );
            assert_eq!(
                (cursor.history.after, cursor.history.ceiling),
                (40, Some(100))
            );
            assert_eq!(cursor.source_identity, "new-process");
            assert_eq!(
                cursor
                    .retries
                    .pending
                    .iter()
                    .map(|retry| retry.id)
                    .collect::<Vec<_>>(),
                [7, 80]
            );
            assert_eq!(cursor.retries.pending[0].ready_at, now);
            assert_eq!(
                cursor.retries.pending[1].ready_at,
                now + Duration::from_secs(wait)
            );
            assert_eq!(cursor.retries.pending[1].attempts, 6);
        }
    }

    #[test]
    fn rejects_foreign_generations_roots_replacements_and_invalid_debt() {
        let (temp, binding) = binding_fixture();
        let now = Instant::now();
        let mut snapshot = Snapshot::capture(binding.clone(), &debt(now), now, 1);
        let mut foreign = binding.clone();
        foreign.generation = Some("ccdd".into());
        assert!(snapshot.validate(&foreign).is_err());
        foreign = binding.clone();
        foreign.archive = temp.path().join("other-archive");
        assert!(snapshot.validate(&foreign).is_err());
        snapshot.retries[1].id = 7;
        assert!(snapshot.validate(&binding).is_err());
        snapshot.retries[1].id = 80;
        snapshot.history.ceiling = Some(39);
        assert!(snapshot.validate(&binding).is_err());
        snapshot.history.ceiling = Some(100);
        snapshot.retries[0].wait_us = 60_000_001;
        assert!(snapshot.validate(&binding).is_err());
        snapshot.retries[0].wait_us = 0;
        #[cfg(windows)]
        let retained =
            mcp_agent_mail_core::disk::open_regular_file_no_follow(&binding.database).unwrap();
        std::fs::rename(&binding.database, temp.path().join("old.sqlite3")).unwrap();
        std::fs::write(&binding.database, b"replacement source").unwrap();
        #[cfg(windows)]
        match_creation_time(&binding.database, &retained);
        let replacement = Binding::at(
            &binding.database,
            &binding.archive,
            binding.generation.clone(),
        )
        .unwrap();
        assert!(snapshot.validate(&replacement).is_err());
    }

    #[test]
    fn owner_is_exclusive_until_drop_and_failed_save_keeps_previous_snapshot() {
        let (_temp, binding) = binding_fixture();
        let store = Store::open(binding.clone()).unwrap();
        let mut cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        assert!(contended(&Store::open(binding.clone()).unwrap_err()));
        let original = std::fs::read(&store.path).unwrap();
        let held = store.path.with_extension("saved");
        std::fs::rename(&store.path, &held).unwrap();
        std::fs::create_dir(&store.path).unwrap();
        cursor.history.after = 60;
        assert!(store.save(&cursor).is_err());
        assert_eq!(std::fs::read(&held).unwrap(), original);
        // Probe the actual held lock, independently of the obstructed snapshot.
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&store.lock_path)
            .unwrap();
        assert!(contended(
            &FileExt::try_lock_exclusive(&contender).unwrap_err()
        ));
        drop(contender);
        std::fs::rename(&store.path, store.path.with_extension("obstruction")).unwrap();
        std::fs::rename(&held, &store.path).unwrap();
        store.save(&cursor).unwrap();
        assert_eq!(store.load().unwrap().unwrap().history.after, 60);
        drop(store);
        assert!(Store::open(binding).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_hardlinks_and_replaced_lock_inodes() {
        let (temp, binding) = binding_fixture();
        let path = binding.path().unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        assert!(Store::open(binding.clone()).is_err());
        std::fs::rename(&path, temp.path().join("held-symlink")).unwrap();
        std::fs::hard_link(&victim, &path).unwrap();
        assert!(Store::open(binding.clone()).is_err());
        std::fs::rename(&path, temp.path().join("held-hardlink")).unwrap();
        let store = Store::open(binding).unwrap();
        std::fs::rename(&store.lock_path, temp.path().join("old-lock")).unwrap();
        std::fs::write(&store.lock_path, b"replacement lock").unwrap();
        assert!(store.save(&debt(Instant::now())).is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_hardlinked_checkpoint_and_lock_paths_fail_closed() {
        for is_lock in [false, true] {
            let (temp, binding) = binding_fixture();
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

    #[cfg(any(unix, windows))]
    #[test]
    fn publication_rechecks_checkpoint_owner_after_source_admission() {
        let (temp, binding) = binding_fixture();
        let store = Store::open(binding).unwrap();
        let cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        let previous = std::fs::read(&store.path).unwrap();
        let old_lock = temp.path().join("old-lock");
        let result = store.save_with(&cursor, || {
            std::fs::rename(&store.lock_path, &old_lock)?;
            std::fs::write(&store.lock_path, b"replacement owner")?;
            #[cfg(windows)]
            match_creation_time(&store.lock_path, &store.lock);
            Ok(())
        });
        #[cfg(unix)]
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("owner inode changed")
        );
        #[cfg(windows)]
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("owner file identity changed")
        );
        assert_eq!(std::fs::read(&store.path).unwrap(), previous);
        assert_eq!(
            std::fs::read(&store.lock_path).unwrap(),
            b"replacement owner"
        );
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(old_lock)
            .unwrap();
        assert!(contended(
            &FileExt::try_lock_exclusive(&contender).unwrap_err()
        ));
    }

    #[test]
    fn full_retry_queue_fits_and_oversized_or_malformed_snapshots_are_refused() {
        let (_temp, binding) = binding_fixture();
        let store = Store::open(binding.clone()).unwrap();
        let now = Instant::now();
        let mut cursor = debt(now);
        cursor.retries.pending.clear();
        for offset in 0..MAX_RETRY_IDS {
            cursor.retries.pending.push_back(DeferredRelease {
                id: i64::MAX - i64::try_from(offset).unwrap(),
                attempts: u32::MAX,
                ready_at: now + MAX_BACKOFF,
            });
        }
        store.save(&cursor).unwrap();
        assert!(std::fs::metadata(&store.path).unwrap().len() <= MAX_BYTES);
        let snapshot = store.load().unwrap().unwrap();
        assert_eq!(snapshot.retries.len(), MAX_RETRY_IDS);
        assert_eq!(snapshot.retries[0].id, i64::MAX);
        cursor.retries.pending.push_back(DeferredRelease {
            id: 1,
            attempts: 0,
            ready_at: now,
        });
        assert!(store.save(&cursor).is_err());
        assert_eq!(store.load().unwrap().unwrap().retries.len(), MAX_RETRY_IDS);
        std::fs::write(&store.path, b"not JSON").unwrap();
        assert!(store.load().is_err());
        std::fs::write(
            &store.path,
            vec![b' '; usize::try_from(MAX_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn checkpoint_api_rejects_stopped_and_query_only_work_before_creating_sidecars() {
        super::super::super::tests::fixture(|cx, pool, config| {
            let binding = Binding::capture(cx, pool, config).unwrap();
            let path = binding.path().unwrap();
            let lock = path.with_extension("json.lock");
            let mut cursor = ReservationReconcileCursor::default();
            let stopped = cursor
                .reconcile_checkpointed(cx, pool, config, &AtomicBool::new(true))
                .unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.scanned, 0);
            assert!(!path.exists() && !lock.exists());
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let error = cursor
                .reconcile_checkpointed(cx, &readonly, config, &AtomicBool::new(false))
                .unwrap_err();
            assert!(error.contains("query-only"));
            assert!(!path.exists() && !lock.exists());
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn recreated_worker_resumes_history_and_repairs_retained_release_from_current_source() {
        super::super::super::tests::fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..482 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'active/{id}.rs', 1, 'active', 1000000, 9000000, NULL)")).unwrap();
            }
            drop(conn);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let process = crate::archive_process_lock(&archive).unwrap();
            let held = process.lock().unwrap();
            let stop = AtomicBool::new(false);
            let mut cursor = ReservationReconcileCursor::default();
            let first = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(
                (first.deferred_transient, first.retry_len, first.repaired),
                (1, 1, 0)
            );
            assert_eq!(cursor.history.after, 432);
            let binding = Binding::capture(cx, pool, config).unwrap();
            let persisted = std::fs::read(binding.path().unwrap()).unwrap();
            let snapshot: Snapshot = serde_json::from_slice(&persisted).unwrap();
            assert_eq!(snapshot.history.after, 432);
            assert_eq!(snapshot.retries[0].id, 401);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
            let mut competing = ReservationReconcileCursor::default();
            assert_eq!(
                competing
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap_err(),
                OWNED
            );
            assert_eq!(competing.history.after, 0);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
            drop(cursor);
            drop(held);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE file_reservations SET reason='current after restart' WHERE id=401",
            )
            .unwrap();
            drop(conn);
            let resumed = competing
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!((resumed.repaired, resumed.retry_len), (1, 0));
            assert!(competing.history.after > 432 && competing.history.after < 481);
            assert!(resumed.scanned <= 32 && resumed.attempted <= 4);
            let relative = "projects/project/file_reservations/id-401-gaabb.json";
            let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["reason"], "current after restart");
            assert_eq!(value["db_generation"], "aabb");
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            let entry = tree.get_path(Path::new(relative)).unwrap();
            assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync(
                    "SELECT released_ts FROM file_reservations WHERE id=401",
                    &[]
                )
                .unwrap()[0]
                    .get_as::<Option<i64>>(0)
                    .unwrap(),
                None
            );
            assert_eq!(
                conn.query_sync(
                    "SELECT released_ts FROM file_reservation_releases WHERE reservation_id=401",
                    &[]
                )
                .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                5_000_000
            );
        });
    }
}
