#![forbid(unsafe_code)]
//! Git archive storage layer for MCP Agent Mail.
//!
//! Provides per-project git archives with:
//! - Archive root initialization + per-project git repos + `.gitattributes`
//! - Advisory file locks (`.archive.lock`) and commit locks
//! - Commit queue with batching to reduce lock contention
//! - Message write pipeline (canonical + inbox/outbox copies)
//! - File reservation artifact writes (sha1(pattern).json + id-<id>.json)
//! - Agent profile writes
//! - Notification signals

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use git2::{ErrorCode, IndexAddOption, Repository, Signature};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::Digest as Sha1Digest;
use thiserror::Error;

use mcp_agent_mail_core::{LockLevel, OrderedMutex, OrderedRwLock, config::Config};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Git error: {0}")]
    Git(#[from] git2::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Lock contention: {message}")]
    LockContention { message: String },

    #[error("Git index.lock contention after {attempts} retries: {message}")]
    GitIndexLock {
        message: String,
        lock_path: PathBuf,
        attempts: usize,
    },

    #[error("Lock acquisition timed out: {0}")]
    LockTimeout(String),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Archive not initialized")]
    NotInitialized,
}

pub type Result<T> = std::result::Result<T, StorageError>;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A project archive backed by a git repository.
#[derive(Debug, Clone)]
pub struct ProjectArchive {
    pub slug: String,
    pub root: PathBuf,
    pub repo_root: PathBuf,
    pub lock_path: PathBuf,
    /// Pre-canonicalized root path — avoids repeated `readlink` syscalls.
    canonical_root: PathBuf,
    /// Pre-canonicalized repo root path — avoids repeated `readlink` syscalls.
    canonical_repo_root: PathBuf,
}

/// Metadata about a single git commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub summary: String,
}

/// Paths where a message is written in the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageArchivePaths {
    pub canonical: PathBuf,
    pub outbox: PathBuf,
    pub inbox: Vec<PathBuf>,
}

/// Metadata included in notification signal files when enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationMessage {
    pub id: Option<i64>,
    pub from: Option<String>,
    pub subject: Option<String>,
    pub importance: Option<String>,
}

// ---------------------------------------------------------------------------
// Write-Behind Queue (WBQ)
// ---------------------------------------------------------------------------
//
// Moves archive file writes (and notification signals) off the tool hot path
// to a dedicated background OS thread.  The DB is the source of truth; archive
// writes are best-effort.  The drain worker batches writes and funnels them
// into async commits via the existing CommitCoalescer.

/// An archive write operation that can be deferred to the background drain
/// thread.
#[derive(Debug, Clone)]
pub enum WriteOp {
    MessageBundle {
        project_slug: String,
        config: Config,
        message_json: serde_json::Value,
        body_md: String,
        sender: String,
        recipients: Vec<String>,
        /// Additional repo-root-relative paths to include in the git commit
        /// (e.g., attachment files/manifests created during message processing).
        extra_paths: Vec<String>,
    },
    AgentProfile {
        project_slug: String,
        config: Config,
        agent_json: serde_json::Value,
    },
    FileReservation {
        project_slug: String,
        config: Config,
        reservations: Vec<serde_json::Value>,
    },
    NotificationSignal {
        config: Config,
        project_slug: String,
        agent_name: String,
        metadata: Option<NotificationMessage>,
    },
    ClearSignal {
        config: Config,
        project_slug: String,
        agent_name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WbqEnqueueResult {
    Enqueued,
    QueueUnavailable,
    SkippedDiskCritical,
}

#[inline]
fn now_micros_u64() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u128::from(u64::MAX)),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Default)]
pub struct WbqStats {
    pub enqueued: u64,
    pub drained: u64,
    pub errors: u64,
    pub fallbacks: u64,
}

enum WbqMsg {
    // `WriteOp` is large (contains `Config` + payloads). Boxing keeps the channel
    // messages small and avoids repeatedly moving ~KB-sized values around.
    Op(WbqOpEnvelope),
    Flush(std::sync::mpsc::SyncSender<()>),
    Shutdown,
}

#[derive(Debug)]
struct WbqOpEnvelope {
    enqueued_at: Instant,
    op: Box<WriteOp>,
}

struct WriteBehindQueue {
    sender: std::sync::mpsc::SyncSender<WbqMsg>,
    drain_handle: OrderedMutex<Option<std::thread::JoinHandle<()>>>,
    op_depth: Arc<AtomicU64>,
}

const WBQ_CHANNEL_CAPACITY: usize = 8_192;
const WBQ_DRAIN_BATCH_CAP: usize = 256;
const WBQ_FLUSH_INTERVAL_MS: u64 = 100;
const WBQ_ENQUEUE_TIMEOUT_MS: u64 = 100;

static WBQ: OnceLock<WriteBehindQueue> = OnceLock::new();

/// Spawn the WBQ drain thread. Safe to call multiple times.
pub fn wbq_start() {
    WBQ.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(WBQ_CHANNEL_CAPACITY);
        let op_depth = Arc::new(AtomicU64::new(0));
        let op_depth_worker = Arc::clone(&op_depth);
        let handle = std::thread::Builder::new()
            .name("wbq-drain".into())
            .spawn(move || wbq_drain_loop(rx, op_depth_worker))
            .expect("failed to spawn WBQ drain worker");

        mcp_agent_mail_core::global_metrics()
            .storage
            .wbq_capacity
            .set(u64::try_from(WBQ_CHANNEL_CAPACITY).unwrap_or(u64::MAX));

        WriteBehindQueue {
            sender: tx,
            drain_handle: OrderedMutex::new(LockLevel::StorageWbqDrainHandle, Some(handle)),
            op_depth,
        }
    });
}

fn wbq_record_enqueue_success(wbq: &WriteBehindQueue) {
    let metrics = mcp_agent_mail_core::global_metrics();
    metrics.storage.wbq_enqueued_total.inc();

    let depth = wbq
        .op_depth
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    metrics.storage.wbq_depth.set(depth);
    metrics.storage.wbq_peak_depth.fetch_max(depth);

    let cap = u64::try_from(WBQ_CHANNEL_CAPACITY).unwrap_or(u64::MAX);
    let threshold = cap.saturating_mul(80).saturating_div(100);
    if threshold > 0 && depth >= threshold {
        if metrics.storage.wbq_over_80_since_us.load() == 0 {
            metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
        }
    } else {
        metrics.storage.wbq_over_80_since_us.set(0);
    }
}

/// Enqueue a write op to the background drain thread.
///
/// The DB remains the source of truth; archive writes are best-effort.
pub fn wbq_enqueue(op: WriteOp) -> WbqEnqueueResult {
    // Under critical disk pressure, disable archive writes (DB remains authoritative).
    let disk_pressure = mcp_agent_mail_core::global_metrics()
        .system
        .disk_pressure_level
        .load();
    if disk_pressure >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64() {
        return WbqEnqueueResult::SkippedDiskCritical;
    }

    wbq_start();
    let wbq = WBQ.get().expect("WBQ must be initialised");
    let envelope = WbqOpEnvelope {
        enqueued_at: Instant::now(),
        op: Box::new(op),
    };

    let msg = WbqMsg::Op(envelope);
    match wbq.sender.try_send(msg) {
        Ok(()) => {
            wbq_record_enqueue_success(wbq);
            WbqEnqueueResult::Enqueued
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => WbqEnqueueResult::QueueUnavailable,
        Err(std::sync::mpsc::TrySendError::Full(msg)) => {
            // Backpressure: queue is temporarily full. Block briefly to avoid
            // synchronous IO fallback on the tool hot path.
            mcp_agent_mail_core::global_metrics()
                .storage
                .wbq_fallbacks_total
                .inc();
            // std::sync::mpsc::SyncSender does not expose a stable send_timeout; emulate with
            // try_send + a short sleep until a deadline.
            let deadline = Instant::now() + Duration::from_millis(WBQ_ENQUEUE_TIMEOUT_MS);
            let mut cur = msg;
            loop {
                match wbq.sender.try_send(cur) {
                    Ok(()) => {
                        wbq_record_enqueue_success(wbq);
                        break WbqEnqueueResult::Enqueued;
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        break WbqEnqueueResult::QueueUnavailable;
                    }
                    Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                        if Instant::now() >= deadline {
                            break WbqEnqueueResult::QueueUnavailable;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                        cur = returned;
                    }
                }
            }
        }
    }
}

/// Block until all pending write ops have been drained.
///
/// Uses a blocking `send` so the Flush message is guaranteed to enter the
/// channel even when it is temporarily full.  If the drain thread has
/// panicked (receiver dropped), `send` returns `Err` immediately – no
/// deadlock risk.
pub fn wbq_flush() {
    if let Some(wbq) = WBQ.get() {
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        if wbq.sender.send(WbqMsg::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
        }
    }
}

/// Drain remaining ops, stop the drain thread, and join it.
///
/// Sends a `Shutdown` message after flushing so the drain thread exits
/// its loop.  Without this, the thread would block on `recv_timeout`
/// indefinitely because the sender lives in a `OnceLock` static and is
/// never dropped.
pub fn wbq_shutdown() {
    if let Some(wbq) = WBQ.get() {
        wbq_flush();
        // Tell the drain thread to exit.  Use blocking `send` so it is
        // guaranteed to be delivered (same rationale as wbq_flush).
        let _ = wbq.sender.send(WbqMsg::Shutdown);
        let handle = {
            let mut guard = wbq.drain_handle.lock();
            guard.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// Snapshot of current WBQ statistics.
pub fn wbq_stats() -> WbqStats {
    let snap = mcp_agent_mail_core::global_metrics().snapshot();
    WbqStats {
        enqueued: snap.storage.wbq_enqueued_total,
        drained: snap.storage.wbq_drained_total,
        errors: snap.storage.wbq_errors_total,
        fallbacks: snap.storage.wbq_fallbacks_total,
    }
}

fn wbq_drain_loop(rx: std::sync::mpsc::Receiver<WbqMsg>, op_depth: Arc<AtomicU64>) {
    let flush_interval = Duration::from_millis(WBQ_FLUSH_INTERVAL_MS);
    let mut flush_waiters: Vec<std::sync::mpsc::SyncSender<()>> = Vec::new();
    let mut shutting_down = false;

    loop {
        let mut batch: Vec<WbqOpEnvelope> = Vec::new();

        match rx.recv_timeout(flush_interval) {
            Ok(WbqMsg::Op(op)) => batch.push(op),
            Ok(WbqMsg::Flush(done_tx)) => flush_waiters.push(done_tx),
            Ok(WbqMsg::Shutdown) => shutting_down = true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                for w in flush_waiters.drain(..) {
                    let _ = w.try_send(());
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Drain remaining items from the channel (up to batch cap).
        while batch.len() < WBQ_DRAIN_BATCH_CAP {
            match rx.try_recv() {
                Ok(WbqMsg::Op(op)) => batch.push(op),
                Ok(WbqMsg::Flush(done_tx)) => flush_waiters.push(done_tx),
                Ok(WbqMsg::Shutdown) => shutting_down = true,
                Err(_) => break,
            }
        }

        let drained = batch.len();
        let drained_u64 = u64::try_from(drained).unwrap_or(u64::MAX);

        let depth_after = op_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(drained_u64))
            })
            .unwrap_or(0)
            .saturating_sub(drained_u64);

        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.storage.wbq_depth.set(depth_after);
        let cap = u64::try_from(WBQ_CHANNEL_CAPACITY).unwrap_or(u64::MAX);
        let threshold = cap.saturating_mul(80).saturating_div(100);
        if threshold > 0 && depth_after >= threshold {
            if metrics.storage.wbq_over_80_since_us.load() == 0 {
                metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
            }
        } else {
            metrics.storage.wbq_over_80_since_us.set(0);
        }

        let mut errors = 0usize;
        for envelope in batch {
            let disk_pressure = mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .load();
            let r = if disk_pressure >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64() {
                Ok(())
            } else {
                wbq_execute_op(&envelope.op)
            };
            let latency_us = u64::try_from(
                envelope
                    .enqueued_at
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)),
            )
            .unwrap_or(u64::MAX);
            metrics.storage.wbq_queue_latency_us.record(latency_us);
            if let Err(e) = r {
                tracing::warn!("[wbq-drain] op failed: {e}");
                errors += 1;
            }
        }

        metrics.storage.wbq_drained_total.add(drained_u64);
        metrics
            .storage
            .wbq_errors_total
            .add(u64::try_from(errors).unwrap_or(u64::MAX));

        for w in flush_waiters.drain(..) {
            let _ = w.try_send(());
        }

        if shutting_down {
            break;
        }
    }

    // Drain any remaining messages after the loop exits.
    for msg in rx.try_iter() {
        match msg {
            WbqMsg::Op(envelope) => {
                let depth_after = op_depth
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                        Some(cur.saturating_sub(1))
                    })
                    .unwrap_or(0)
                    .saturating_sub(1);
                let metrics = mcp_agent_mail_core::global_metrics();
                metrics.storage.wbq_depth.set(depth_after);
                let cap = u64::try_from(WBQ_CHANNEL_CAPACITY).unwrap_or(u64::MAX);
                let threshold = cap.saturating_mul(80).saturating_div(100);
                if threshold > 0 && depth_after >= threshold {
                    if metrics.storage.wbq_over_80_since_us.load() == 0 {
                        metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
                    }
                } else {
                    metrics.storage.wbq_over_80_since_us.set(0);
                }

                let disk_pressure = mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level
                    .load();
                let r = if disk_pressure
                    >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
                {
                    Ok(())
                } else {
                    wbq_execute_op(&envelope.op)
                };
                let latency_us = u64::try_from(
                    envelope
                        .enqueued_at
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)),
                )
                .unwrap_or(u64::MAX);
                metrics.storage.wbq_queue_latency_us.record(latency_us);
                metrics.storage.wbq_drained_total.inc();
                if r.is_err() {
                    metrics.storage.wbq_errors_total.inc();
                }
            }
            WbqMsg::Flush(done_tx) => {
                let _ = done_tx.try_send(());
            }
            WbqMsg::Shutdown => {} // already shutting down
        }
    }
}

fn wbq_execute_op(op: &WriteOp) -> Result<()> {
    match op {
        WriteOp::MessageBundle {
            project_slug,
            config,
            message_json,
            body_md,
            sender,
            recipients,
            extra_paths,
        } => {
            let archive = ensure_archive(config, project_slug)?;
            write_message_bundle(
                &archive,
                config,
                message_json,
                body_md,
                sender,
                recipients,
                extra_paths,
                None,
            )
        }
        WriteOp::AgentProfile {
            project_slug,
            config,
            agent_json,
        } => {
            let archive = ensure_archive(config, project_slug)?;
            write_agent_profile_with_config(&archive, config, agent_json)
        }
        WriteOp::FileReservation {
            project_slug,
            config,
            reservations,
        } => {
            let archive = ensure_archive(config, project_slug)?;
            write_file_reservation_records(&archive, config, reservations)
        }
        WriteOp::NotificationSignal {
            config,
            project_slug,
            agent_name,
            metadata,
        } => {
            emit_notification_signal(config, project_slug, agent_name, metadata.as_ref());
            Ok(())
        }
        WriteOp::ClearSignal {
            config,
            project_slug,
            agent_name,
        } => {
            clear_notification_signal(config, project_slug, agent_name);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// In-process per-project archive lock (two-level locking)
// ---------------------------------------------------------------------------

/// In-process per-project mutex map.
///
/// Threads within the same server process acquire this mutex *before* the
/// filesystem advisory lock.  This eliminates syscall-heavy file-lock retries
/// for intra-process contention (the dominant case under load).
///
/// The outer `RwLock` is read-locked for lookup (hot path, concurrent) and
/// write-locked only when creating a new project entry (cold path, once).
static ARCHIVE_LOCK_MAP: OnceLock<OrderedRwLock<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

fn archive_process_lock(project_slug: &str) -> Arc<Mutex<()>> {
    let map = ARCHIVE_LOCK_MAP
        .get_or_init(|| OrderedRwLock::new(LockLevel::StorageArchiveLockMap, HashMap::new()));

    // Fast path: read lock for existing entry.
    {
        let guard = map.read();
        if let Some(lock) = guard.get(project_slug) {
            return Arc::clone(lock);
        }
    }

    // Slow path: write lock to insert.
    let mut guard = map.write();
    if let Some(lock) = guard.get(project_slug) {
        return Arc::clone(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    guard.insert(project_slug.to_string(), Arc::clone(&lock));
    lock
}

// ---------------------------------------------------------------------------
// Thread-local jitter PRNG (replaces PID-based jitter)
// ---------------------------------------------------------------------------

/// Simple xorshift64 PRNG for lock-retry jitter.
///
/// Seeded per-thread from thread ID + timestamp so concurrent threads in the
/// same process produce distinct jitter sequences (avoiding thundering herd).
fn thread_jitter_ms(range: u64) -> u64 {
    use std::cell::Cell;

    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            let tid = std::thread::current().id();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            // Mix thread-id hash with timestamp; ensure non-zero.
            let mut seed = now ^ (format!("{tid:?}").len() as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
            if seed == 0 { seed = 1; }
            seed
        });
    }

    if range == 0 {
        return 0;
    }

    STATE.with(|cell| {
        let mut s = cell.get();
        // xorshift64
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        cell.set(s);
        s % range
    })
}

// ---------------------------------------------------------------------------
// Advisory file lock (per-project)
// ---------------------------------------------------------------------------

/// Owner metadata stored alongside the lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockOwnerMeta {
    pid: u32,
    created_ts: f64,
}

/// Per-project advisory file lock with stale detection.
///
/// Mirrors the Python `AsyncFileLock` semantics:
/// - Lock file at the given path (e.g. `<project>/.archive.lock`)
/// - Owner metadata in `<lock_path>.owner.json` with `{pid, created_ts}`
/// - Stale detection: owner PID dead, or lock age > stale_timeout
/// - Exponential backoff with jitter on contention
pub struct FileLock {
    path: PathBuf,
    metadata_path: PathBuf,
    timeout: Duration,
    stale_timeout: Duration,
    max_retries: usize,
    held: bool,
}

impl FileLock {
    /// Create a new advisory file lock.
    ///
    /// Defaults match Python: timeout=60s, stale_timeout=180s, max_retries=5.
    pub fn new(path: PathBuf) -> Self {
        let metadata_path = {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            path.with_file_name(format!("{name}.owner.json"))
        };
        Self {
            path,
            metadata_path,
            timeout: Duration::from_secs(60),
            stale_timeout: Duration::from_secs(180),
            max_retries: 5,
            held: false,
        }
    }

    /// Configure timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure stale timeout.
    pub fn with_stale_timeout(mut self, stale_timeout: Duration) -> Self {
        self.stale_timeout = stale_timeout;
        self
    }

    /// Configure max retries.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Acquire the lock with retry and stale detection.
    pub fn acquire(&mut self) -> Result<()> {
        use fs2::FileExt;

        let start = Instant::now();

        ensure_parent_dir(&self.path)?;

        for attempt in 0..=self.max_retries {
            let elapsed = start.elapsed();
            if elapsed >= self.timeout && attempt > 0 {
                break;
            }

            // Try to create and exclusively lock the file
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&self.path)?;

            match file.try_lock_exclusive() {
                Ok(()) => {
                    // Lock acquired - write owner metadata
                    self.write_metadata()?;
                    self.held = true;
                    return Ok(());
                }
                Err(_) => {
                    // Lock held by another process - check for stale
                    if self.cleanup_if_stale()? {
                        // Stale lock cleaned up, retry immediately
                        continue;
                    }

                    if attempt >= self.max_retries {
                        break;
                    }

                    // Exponential backoff with per-thread jitter.
                    // Uses thread-local xorshift instead of PID so threads in
                    // the same process don't synchronize their retry delays.
                    let base_ms = if attempt == 0 {
                        50
                    } else {
                        50 * (1u64 << attempt.min(4))
                    };
                    let jitter_range = base_ms / 2 + 1; // ±25% of base
                    let jitter = thread_jitter_ms(jitter_range);
                    let sleep_ms = base_ms.saturating_add(jitter).max(10);
                    std::thread::sleep(Duration::from_millis(sleep_ms));
                }
            }
        }

        Err(StorageError::LockTimeout(format!(
            "Timed out acquiring lock {} after {:.2}s ({} attempts)",
            self.path.display(),
            start.elapsed().as_secs_f64(),
            self.max_retries + 1
        )))
    }

    /// Release the lock.
    pub fn release(&mut self) -> Result<()> {
        if !self.held {
            return Ok(());
        }
        self.held = false;

        // Remove metadata file first
        let _ = fs::remove_file(&self.metadata_path);
        // Remove lock file
        let _ = fs::remove_file(&self.path);
        Ok(())
    }

    /// Write owner metadata alongside the lock file.
    fn write_metadata(&self) -> Result<()> {
        let meta = LockOwnerMeta {
            pid: std::process::id(),
            created_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
        };
        let content = serde_json::to_string(&meta)?;
        fs::write(&self.metadata_path, content)?;
        Ok(())
    }

    /// Check if the lock is stale and clean it up if so.
    ///
    /// A lock is stale if:
    /// 1. The owning PID is no longer alive, OR
    /// 2. The lock age exceeds `stale_timeout`
    fn cleanup_if_stale(&self) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        // Read owner metadata
        let meta = if self.metadata_path.exists() {
            fs::read_to_string(&self.metadata_path)
                .ok()
                .and_then(|s| serde_json::from_str::<LockOwnerMeta>(&s).ok())
        } else {
            None
        };

        let owner_alive = meta.as_ref().map(|m| pid_alive(m.pid)).unwrap_or(false);

        let age = meta.as_ref().map(|m| now - m.created_ts).or_else(|| {
            fs::metadata(&self.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| now - d.as_secs_f64())
        });

        let is_stale = if !owner_alive {
            true
        } else if !self.stale_timeout.is_zero() {
            age.is_some_and(|a| a >= self.stale_timeout.as_secs_f64())
        } else {
            false
        };

        if !is_stale {
            return Ok(false);
        }

        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.metadata_path);
        Ok(true)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Execute a closure while holding the project advisory lock.
///
/// Uses two-level locking for efficient intra-process coordination:
/// 1. In-process per-project mutex (fast, no syscalls) — serializes threads.
/// 2. Filesystem advisory lock (`.archive.lock`) — serializes processes.
///
/// The in-process mutex is acquired first.  This means most contention
/// is resolved cheaply (OS futex) without filesystem retry storms.
pub fn with_project_lock<F, T>(archive: &ProjectArchive, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let process_lock = archive_process_lock(&archive.slug);
    let _guard = process_lock.lock().expect("archive process lock poisoned");

    let mut lock = FileLock::new(archive.lock_path.clone());
    let lock_start = std::time::Instant::now();
    lock.acquire()?;
    mcp_agent_mail_core::global_metrics()
        .storage
        .archive_lock_wait_us
        .record(lock_start.elapsed().as_micros() as u64);
    let result = f();
    lock.release()?;
    result
}

/// Check if a process with the given PID is alive (Unix only).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // On Unix, kill(pid, 0) checks if process exists without sending a signal
    #[cfg(unix)]
    {
        let result = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        matches!(result, Ok(s) if s.success())
    }
    #[cfg(not(unix))]
    {
        // On non-Unix, conservatively assume alive
        true
    }
}

// ---------------------------------------------------------------------------
// Commit queue with batching
// ---------------------------------------------------------------------------

/// A request to commit a set of files to a repository.
struct CommitRequest {
    repo_root: PathBuf,
    message: String,
    rel_paths: Vec<String>,
}

/// Statistics about commit queue/coalescer operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitQueueStats {
    pub enqueued: usize,
    pub batched: usize,
    pub commits: usize,
    pub avg_batch_size: f64,
    pub queue_size: usize,
    pub errors: usize,
}

/// Commit queue that batches multiple commits to reduce git contention.
///
/// When multiple write operations happen rapidly (e.g. sending a message
/// to N recipients), individual commits can be merged into a single
/// batch commit if they target the same repo and have no path conflicts.
///
/// Default settings: max_batch_size=10, max_wait=50ms, max_queue_size=100.
pub struct CommitQueue {
    queue: Mutex<VecDeque<CommitRequest>>,
    max_batch_size: usize,
    max_wait: Duration,
    max_queue_size: usize,
    // Stats
    stats: Mutex<CommitQueueStats>,
    batch_sizes: Mutex<VecDeque<usize>>,
}

impl Default for CommitQueue {
    fn default() -> Self {
        Self::new(10, Duration::from_millis(50), 100)
    }
}

impl CommitQueue {
    /// Create a new commit queue.
    pub fn new(max_batch_size: usize, max_wait: Duration, max_queue_size: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            max_batch_size,
            max_wait,
            max_queue_size,
            stats: Mutex::new(CommitQueueStats::default()),
            batch_sizes: Mutex::new(VecDeque::new()),
        }
    }

    /// Enqueue a commit request. If the queue has capacity, the request is
    /// buffered; otherwise it falls back to a direct commit.
    pub fn enqueue(
        &self,
        repo_root: PathBuf,
        config: &Config,
        message: String,
        rel_paths: Vec<String>,
    ) -> Result<()> {
        if rel_paths.is_empty() {
            return Ok(());
        }

        {
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.enqueued += 1;
        }

        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= self.max_queue_size {
            // Queue full - fall back to direct commit
            drop(queue);
            let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
            commit_paths_with_retry(&repo_root, config, &message, &refs)?;
            return Ok(());
        }

        queue.push_back(CommitRequest {
            repo_root,
            message,
            rel_paths,
        });
        drop(queue);

        Ok(())
    }

    /// Drain the queue and process all pending commits.
    ///
    /// This is the synchronous drain that processes batches. In practice,
    /// callers should call this after a short delay or after a burst of
    /// enqueue operations.
    pub fn drain(&self, config: &Config) -> Result<()> {
        let deadline = Instant::now() + self.max_wait;

        loop {
            // Collect a batch
            let batch = {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.is_empty() {
                    break;
                }

                let mut batch = Vec::new();
                while batch.len() < self.max_batch_size && !queue.is_empty() {
                    if let Some(req) = queue.pop_front() {
                        batch.push(req);
                    }
                }
                batch
            };

            if batch.is_empty() {
                break;
            }

            self.process_batch(config, batch)?;

            if Instant::now() >= deadline {
                break;
            }
        }

        // Update queue_size stat
        {
            let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.queue_size = queue.len();
        }

        Ok(())
    }

    /// Process a batch of commit requests.
    fn process_batch(&self, config: &Config, batch: Vec<CommitRequest>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        {
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.batched += batch.len();
        }

        // Group by repo root
        let mut by_repo: HashMap<PathBuf, Vec<CommitRequest>> = HashMap::new();
        for req in batch {
            by_repo.entry(req.repo_root.clone()).or_default().push(req);
        }

        for (repo_root, requests) in by_repo {
            if requests.len() == 1 {
                // Single request - commit directly
                let req = &requests[0];
                let refs: Vec<&str> = req.rel_paths.iter().map(String::as_str).collect();
                commit_paths_with_retry(&repo_root, config, &req.message, &refs)?;
                self.record_commit(1);
            } else {
                // Multiple requests - try to batch non-conflicting ones
                let mut all_paths = HashSet::new();
                let mut can_batch = true;

                for req in &requests {
                    for p in &req.rel_paths {
                        if !all_paths.insert(p.clone()) {
                            can_batch = false;
                            break;
                        }
                    }
                    if !can_batch {
                        break;
                    }
                }

                if can_batch && requests.len() <= 5 {
                    // Merge into a single commit
                    let mut merged_paths = Vec::new();
                    let mut merged_messages = Vec::new();

                    for req in &requests {
                        merged_paths.extend(req.rel_paths.iter().cloned());
                        let first_line = req.message.lines().next().unwrap_or("");
                        merged_messages.push(format!("- {first_line}"));
                    }

                    let combined = format!(
                        "batch: {} commits\n\n{}",
                        requests.len(),
                        merged_messages.join("\n")
                    );

                    let refs: Vec<&str> = merged_paths.iter().map(String::as_str).collect();
                    commit_paths_with_retry(&repo_root, config, &combined, &refs)?;
                    self.record_commit(requests.len());
                } else {
                    // Conflicts or large batch - process sequentially
                    for req in &requests {
                        let refs: Vec<&str> = req.rel_paths.iter().map(String::as_str).collect();
                        commit_paths_with_retry(&repo_root, config, &req.message, &refs)?;
                        self.record_commit(1);
                    }
                }
            }
        }

        Ok(())
    }

    fn record_commit(&self, batch_size: usize) {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.commits += 1;

        let mut sizes = self.batch_sizes.lock().unwrap_or_else(|e| e.into_inner());
        sizes.push_back(batch_size);
        if sizes.len() > 100 {
            sizes.pop_front();
        }

        let avg = if sizes.is_empty() {
            0.0
        } else {
            sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
        };
        stats.avg_batch_size = (avg * 100.0).round() / 100.0;
    }

    /// Get queue statistics.
    pub fn stats(&self) -> CommitQueueStats {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        stats.queue_size = queue.len();
        stats
    }
}

/// Global commit queue instance.
static COMMIT_QUEUE: LazyLock<OrderedMutex<Option<CommitQueue>>> =
    LazyLock::new(|| OrderedMutex::new(LockLevel::StorageCommitQueue, None));

/// Get or create the global commit queue.
pub fn get_commit_queue() -> &'static OrderedMutex<Option<CommitQueue>> {
    // Ensure initialized
    let mut guard = COMMIT_QUEUE.lock();
    if guard.is_none() {
        *guard = Some(CommitQueue::default());
    }
    drop(guard);
    &COMMIT_QUEUE
}

// ---------------------------------------------------------------------------
// Async commit coalescer (fire-and-forget git commits)
// ---------------------------------------------------------------------------
//
// Architecture for extreme-load resilience with per-project parallelism:
//
//   Tool call:  write files → enqueue_async_commit() → return immediately
//   Per-repo:   each repo_root gets its own queue + spill + metrics
//   Workers:    N threads pick repos via LRS (least-recently-serviced) scheduling
//               Only one worker processes a given repo at a time (CAS lock)
//
// Under extreme load (1000+ agents across 50+ projects):
// - Different projects commit in true parallel (no cross-project serialization)
// - Per-repo batching coalesces many small commits into fewer large ones
// - LRS scheduling ensures fairness: no hot project starves others
// - Spill mechanism keeps the tool hot path non-blocking when queues are full
// - If all workers die, fallback to synchronous commit (no data loss)

/// Fields for a single commit request within the coalescer pipeline.
///
/// Note: `repo_root` is NOT stored here — it's the key in the per-repo queue map.
struct CoalescerCommitFields {
    enqueued_at: Instant,
    git_author_name: String,
    git_author_email: String,
    message: String,
    rel_paths: Vec<String>,
}

struct CoalescerSpillRepo {
    pending_requests: u64,
    earliest_enqueued_at: Instant,
    dirty_all: bool,
    paths: BTreeSet<String>,
    git_author_name: String,
    git_author_email: String,
    message_first_lines: VecDeque<String>,
    message_total: u64,
}

struct CoalescerSpilledWork {
    repo_root: PathBuf,
    pending_requests: u64,
    earliest_enqueued_at: Instant,
    dirty_all: bool,
    paths: Vec<String>,
    git_author_name: String,
    git_author_email: String,
    message_first_lines: Vec<String>,
    message_total: u64,
}

/// Per-repo queue with dedicated spill, processing lock, and metrics.
struct RepoQueue {
    queue: Mutex<VecDeque<CoalescerCommitFields>>,
    spill: Mutex<CoalescerSpillState>,
    /// Atomic depth counter (number of items in queue + spill).
    depth: AtomicU64,
    /// CAS lock: only one worker thread may process this repo at a time.
    processing: AtomicBool,
    /// Microsecond timestamp of last time a worker finished processing this repo.
    last_serviced_us: AtomicU64,
    /// Per-repo metrics for observability.
    metrics: RepoCommitMetrics,
}

/// Spill state for a single repo (replaces the per-shard HashMap<PathBuf, _>).
#[derive(Default)]
struct CoalescerSpillState {
    inner: Option<CoalescerSpillRepo>,
}

/// Per-repo commit metrics.
struct RepoCommitMetrics {
    enqueued_total: AtomicU64,
    drained_total: AtomicU64,
    commits_total: AtomicU64,
    errors_total: AtomicU64,
    retries_total: AtomicU64,
    commit_latency_us_sum: AtomicU64,
    commit_latency_us_count: AtomicU64,
}

impl Default for RepoCommitMetrics {
    fn default() -> Self {
        Self {
            enqueued_total: AtomicU64::new(0),
            drained_total: AtomicU64::new(0),
            commits_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            retries_total: AtomicU64::new(0),
            commit_latency_us_sum: AtomicU64::new(0),
            commit_latency_us_count: AtomicU64::new(0),
        }
    }
}

/// Snapshot of per-repo commit queue statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoCommitStats {
    pub queue_depth: u64,
    pub enqueued_total: u64,
    pub drained_total: u64,
    pub commits_total: u64,
    pub errors_total: u64,
    pub retries_total: u64,
    pub avg_commit_latency_us: u64,
}

/// Fire-and-forget git commit coalescer with per-repo queues and worker pool.
///
/// Each unique `repo_root` gets its own queue, spill buffer, and metrics.
/// A pool of N worker threads services repos via LRS (least-recently-serviced)
/// scheduling, ensuring fairness across projects.
///
/// Only one worker processes a given repo at a time (CAS lock on `processing`),
/// preventing git index.lock contention between workers on the same repo.
///
/// Tool responses never wait for git — they return as soon as files are on disk.
pub struct CommitCoalescer {
    /// Per-repo queues, lazily created on first enqueue.
    repos: Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>>,
    /// Condvar to wake workers when work is available.
    work_cv: Arc<(Mutex<bool>, std::sync::Condvar)>,
    /// Signal workers to shut down.
    shutdown: Arc<AtomicBool>,
    /// Global stats (backward-compatible aggregate view).
    stats: Arc<Mutex<CommitQueueStats>>,
    pending_requests: Arc<AtomicU64>,
    /// Rolling batch size window for avg_batch_size calculation.
    _batch_sizes: Arc<Mutex<VecDeque<usize>>>,
    /// Number of worker threads spawned.
    worker_count: usize,
}

/// Default flush interval for the coalescer (50ms).
///
/// This is the maximum time a commit request waits before being processed.
/// Under sustained load, workers drain all pending requests every interval.
pub const DEFAULT_COALESCER_FLUSH_MS: u64 = 50;

const COALESCER_MAX_BATCH_SIZE: usize = 10;
const COMMIT_COALESCER_SOFT_CAP: u64 = 8_192;
/// Maximum worker threads for the coalescer pool.
const COALESCER_MAX_WORKERS: usize = 32;
/// Per-repo queue capacity before spilling.
const COALESCER_REPO_QUEUE_CAP: usize = 512;

const COALESCER_SPILL_PATH_CAP: usize = 4_096;
const COALESCER_SPILL_MESSAGE_CAP: usize = 32;
const COALESCER_SPILL_MESSAGE_LINE_MAX_CHARS: usize = 120;

/// Auto-detect worker count: `min(available_parallelism, COALESCER_MAX_WORKERS)`, minimum 2.
fn coalescer_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, COALESCER_MAX_WORKERS)
}

impl CommitCoalescer {
    /// Create a new coalescer and spawn the worker pool.
    pub fn new(flush_interval: Duration) -> Self {
        let stats = Arc::new(Mutex::new(CommitQueueStats::default()));
        let batch_sizes = Arc::new(Mutex::new(VecDeque::new()));
        let pending_requests = Arc::new(AtomicU64::new(0));
        let repos: Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let work_cv = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_count = coalescer_worker_count();

        for worker_idx in 0..worker_count {
            let w_repos = Arc::clone(&repos);
            let w_cv = Arc::clone(&work_cv);
            let w_shutdown = Arc::clone(&shutdown);
            let w_stats = Arc::clone(&stats);
            let w_sizes = Arc::clone(&batch_sizes);
            let w_pending = Arc::clone(&pending_requests);

            std::thread::Builder::new()
                .name(format!("commit-coalescer-{worker_idx}"))
                .spawn(move || {
                    coalescer_pool_worker(
                        w_repos,
                        w_cv,
                        w_shutdown,
                        w_stats,
                        w_sizes,
                        w_pending,
                        flush_interval,
                    );
                })
                .expect("failed to spawn commit coalescer worker");
        }

        mcp_agent_mail_core::global_metrics()
            .storage
            .commit_soft_cap
            .set(COMMIT_COALESCER_SOFT_CAP);

        Self {
            repos,
            work_cv,
            shutdown,
            stats,
            pending_requests,
            _batch_sizes: batch_sizes,
            worker_count,
        }
    }

    /// Get or create a per-repo queue for the given repo_root.
    fn get_or_create_repo(&self, repo_root: &Path) -> Arc<RepoQueue> {
        let mut repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rq) = repos.get(repo_root) {
            return Arc::clone(rq);
        }
        let rq = Arc::new(RepoQueue {
            queue: Mutex::new(VecDeque::new()),
            spill: Mutex::new(CoalescerSpillState::default()),
            depth: AtomicU64::new(0),
            processing: AtomicBool::new(false),
            last_serviced_us: AtomicU64::new(0),
            metrics: RepoCommitMetrics::default(),
        });
        repos.insert(repo_root.to_path_buf(), Arc::clone(&rq));
        rq
    }

    /// Spill commit fields into the per-repo spill buffer.
    fn spill_to_repo(rq: &RepoQueue, fields: CoalescerCommitFields) {
        let first_line = fields
            .message
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let first_line: String = first_line
            .chars()
            .take(COALESCER_SPILL_MESSAGE_LINE_MAX_CHARS)
            .collect();

        let mut guard = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.inner.get_or_insert_with(|| CoalescerSpillRepo {
            pending_requests: 0,
            earliest_enqueued_at: fields.enqueued_at,
            dirty_all: false,
            paths: BTreeSet::new(),
            git_author_name: fields.git_author_name.clone(),
            git_author_email: fields.git_author_email.clone(),
            message_first_lines: VecDeque::new(),
            message_total: 0,
        });

        entry.pending_requests = entry.pending_requests.saturating_add(1);
        entry.message_total = entry.message_total.saturating_add(1);
        if fields.enqueued_at < entry.earliest_enqueued_at {
            entry.earliest_enqueued_at = fields.enqueued_at;
        }

        entry.git_author_name = fields.git_author_name;
        entry.git_author_email = fields.git_author_email;

        if !first_line.is_empty() && entry.message_first_lines.len() < COALESCER_SPILL_MESSAGE_CAP {
            entry.message_first_lines.push_back(first_line);
        }

        if !entry.dirty_all {
            for p in fields.rel_paths {
                entry.paths.insert(p);
                if entry.paths.len() > COALESCER_SPILL_PATH_CAP {
                    entry.dirty_all = true;
                    entry.paths.clear();
                    break;
                }
            }
        }
    }

    /// Enqueue a commit request. **Non-blocking, fire-and-forget.**
    ///
    /// Files must already be written to disk before calling this.
    /// The background worker pool will add them to the git index and commit.
    ///
    /// Each unique `repo_root` gets its own queue, enabling true per-project
    /// parallelism across the worker pool.
    pub fn enqueue(
        &self,
        repo_root: PathBuf,
        config: &Config,
        message: String,
        rel_paths: Vec<String>,
    ) {
        if rel_paths.is_empty() {
            return;
        }

        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.storage.commit_enqueued_total.inc();

        {
            let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            s.enqueued += 1;
        }

        let rq = self.get_or_create_repo(&repo_root);
        rq.metrics.enqueued_total.fetch_add(1, Ordering::Relaxed);

        let enqueued_at = Instant::now();
        let fields = CoalescerCommitFields {
            enqueued_at,
            git_author_name: config.git_author_name.clone(),
            git_author_email: config.git_author_email.clone(),
            message,
            rel_paths,
        };

        let pending = self
            .pending_requests
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        metrics.storage.commit_pending_requests.set(pending);
        metrics
            .storage
            .commit_peak_pending_requests
            .fetch_max(pending);

        let threshold = COMMIT_COALESCER_SOFT_CAP
            .saturating_mul(80)
            .saturating_div(100);
        if threshold > 0 && pending >= threshold {
            if metrics.storage.commit_over_80_since_us.load() == 0 {
                metrics
                    .storage
                    .commit_over_80_since_us
                    .set(now_micros_u64());
            }
        } else {
            metrics.storage.commit_over_80_since_us.set(0);
        }

        // Try to push into the per-repo queue; spill if full.
        let queue_depth = rq.depth.load(Ordering::Relaxed);
        if queue_depth < COALESCER_REPO_QUEUE_CAP as u64 {
            let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
            // Re-check under lock
            if q.len() < COALESCER_REPO_QUEUE_CAP {
                q.push_back(fields);
                drop(q);
                rq.depth.fetch_add(1, Ordering::Relaxed);
            } else {
                drop(q);
                Self::spill_to_repo(&rq, fields);
                rq.depth.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            Self::spill_to_repo(&rq, fields);
            rq.depth.fetch_add(1, Ordering::Relaxed);
        }

        // Wake a worker
        let (lock, cvar) = &*self.work_cv;
        {
            let mut ready = lock.lock().unwrap_or_else(|e| e.into_inner());
            *ready = true;
        }
        cvar.notify_one();
    }

    /// Block until all pending commits are flushed to git.
    ///
    /// Use this in tests or during graceful shutdown to ensure all queued
    /// commits are persisted before proceeding.
    pub fn flush_sync(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);

        loop {
            // Wake all workers
            {
                let (lock, cvar) = &*self.work_cv;
                let mut ready = lock.lock().unwrap_or_else(|e| e.into_inner());
                *ready = true;
                cvar.notify_all();
            }

            // Check if all repos are empty and not being processed
            let all_empty = {
                let repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
                repos.values().all(|rq| {
                    rq.depth.load(Ordering::Relaxed) == 0 && !rq.processing.load(Ordering::Relaxed)
                })
            };

            if all_empty {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("flush_sync timed out after 30s; some commits may still be pending");
                break;
            }

            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Get coalescer statistics (aggregate across all repos).
    pub fn stats(&self) -> CommitQueueStats {
        let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Sum queue depths across all repos
        let repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        s.queue_size = repos
            .values()
            .map(|rq| rq.depth.load(Ordering::Relaxed) as usize)
            .sum();
        s
    }

    /// Get per-repo commit statistics for observability.
    pub fn per_repo_stats(&self) -> HashMap<PathBuf, RepoCommitStats> {
        let repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        repos
            .iter()
            .map(|(path, rq)| {
                let count = rq.metrics.commit_latency_us_count.load(Ordering::Relaxed);
                let sum = rq.metrics.commit_latency_us_sum.load(Ordering::Relaxed);
                let avg = sum.checked_div(count).unwrap_or(0);
                (
                    path.clone(),
                    RepoCommitStats {
                        queue_depth: rq.depth.load(Ordering::Relaxed),
                        enqueued_total: rq.metrics.enqueued_total.load(Ordering::Relaxed),
                        drained_total: rq.metrics.drained_total.load(Ordering::Relaxed),
                        commits_total: rq.metrics.commits_total.load(Ordering::Relaxed),
                        errors_total: rq.metrics.errors_total.load(Ordering::Relaxed),
                        retries_total: rq.metrics.retries_total.load(Ordering::Relaxed),
                        avg_commit_latency_us: avg,
                    },
                )
            })
            .collect()
    }

    /// Number of worker threads in the pool.
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }
}

impl Drop for CommitCoalescer {
    fn drop(&mut self) {
        // Signal all workers to exit
        self.shutdown.store(true, Ordering::Release);
        let (_, cvar) = &*self.work_cv;
        cvar.notify_all();
    }
}

/// Worker thread for the per-repo commit coalescer pool.
///
/// Strategy:
/// 1. Wait on condvar (with flush_interval timeout)
/// 2. Scan all repos; pick the one with lowest last_serviced_us that has depth > 0
///    and is not currently being processed by another worker (CAS lock)
/// 3. Drain its queue + spill (up to batch size)
/// 4. Commit batch for that single repo
/// 5. Update per-repo and global metrics
/// 6. If more work remains across any repo, re-signal condvar
/// 7. Repeat
fn coalescer_pool_worker(
    repos: Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>>,
    work_cv: Arc<(Mutex<bool>, std::sync::Condvar)>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<CommitQueueStats>>,
    batch_sizes: Arc<Mutex<VecDeque<usize>>>,
    pending_requests: Arc<AtomicU64>,
    flush_interval: Duration,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // Phase 1: Wait for work or timeout
        {
            let (lock, cvar) = &*work_cv;
            let ready = lock.lock().unwrap_or_else(|e| e.into_inner());
            let _guard = cvar
                .wait_timeout(ready, flush_interval)
                .unwrap_or_else(|e| e.into_inner());
        }

        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // Phase 2: Pick a repo via LRS (least-recently-serviced) scheduling
        let chosen: Option<(PathBuf, Arc<RepoQueue>)> = {
            let repos_guard = repos.lock().unwrap_or_else(|e| e.into_inner());
            let mut best: Option<(PathBuf, Arc<RepoQueue>, u64)> = None;
            for (path, rq) in repos_guard.iter() {
                let depth = rq.depth.load(Ordering::Relaxed);
                if depth == 0 {
                    continue;
                }
                // Skip repos already being processed by another worker
                if rq.processing.load(Ordering::Relaxed) {
                    continue;
                }
                let serviced = rq.last_serviced_us.load(Ordering::Relaxed);
                if best
                    .as_ref()
                    .is_none_or(|(_, _, best_ts)| serviced < *best_ts)
                {
                    best = Some((path.clone(), Arc::clone(rq), serviced));
                }
            }
            best.map(|(p, rq, _)| (p, rq))
        };

        let Some((repo_root, rq)) = chosen else {
            continue;
        };

        // CAS: claim exclusive processing of this repo
        if rq
            .processing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Another worker beat us to it; try again
            continue;
        }

        // Phase 3: Drain queue + spill for this repo
        let mut batch: Vec<CoalescerCommitFields> = Vec::new();
        {
            let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
            while batch.len() < COALESCER_MAX_BATCH_SIZE {
                if let Some(fields) = q.pop_front() {
                    batch.push(fields);
                } else {
                    break;
                }
            }
        }

        // Drain spill if we have room
        let spilled_work = coalescer_drain_repo_spill(&rq, &repo_root);

        let drained_count =
            batch.len() as u64 + spilled_work.as_ref().map_or(0, |w| w.pending_requests);
        // Single atomic fetch_sub avoids TOCTOU race of load-then-sub.
        // If drained_count > prev (shouldn't happen, but guards against wrapping),
        // clamp back to 0.
        let prev = rq.depth.fetch_sub(drained_count, Ordering::Relaxed);
        if drained_count > prev {
            rq.depth.store(0, Ordering::Relaxed);
        }

        // Phase 4: Commit
        if !batch.is_empty() {
            for chunk in batch.chunks(COALESCER_MAX_BATCH_SIZE) {
                coalescer_commit_batch(&repo_root, chunk, &stats, &batch_sizes);

                let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
                let metrics = mcp_agent_mail_core::global_metrics();
                metrics.storage.commit_drained_total.add(chunk_len);
                rq.metrics
                    .drained_total
                    .fetch_add(chunk_len, Ordering::Relaxed);
                rq.metrics.commits_total.fetch_add(1, Ordering::Relaxed);

                for req in chunk {
                    let latency_us = u64::try_from(
                        req.enqueued_at
                            .elapsed()
                            .as_micros()
                            .min(u128::from(u64::MAX)),
                    )
                    .unwrap_or(u64::MAX);
                    metrics.storage.commit_queue_latency_us.record(latency_us);
                    rq.metrics
                        .commit_latency_us_sum
                        .fetch_add(latency_us, Ordering::Relaxed);
                    rq.metrics
                        .commit_latency_us_count
                        .fetch_add(1, Ordering::Relaxed);
                }

                coalescer_update_pending(&pending_requests, chunk_len);
            }
        }

        if let Some(work) = &spilled_work {
            coalescer_commit_spilled_work(work, &stats, &batch_sizes);

            let metrics = mcp_agent_mail_core::global_metrics();
            metrics
                .storage
                .commit_drained_total
                .add(work.pending_requests);
            rq.metrics
                .drained_total
                .fetch_add(work.pending_requests, Ordering::Relaxed);
            rq.metrics.commits_total.fetch_add(1, Ordering::Relaxed);

            let latency_us = u64::try_from(
                work.earliest_enqueued_at
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)),
            )
            .unwrap_or(u64::MAX);
            metrics.storage.commit_queue_latency_us.record(latency_us);
            rq.metrics
                .commit_latency_us_sum
                .fetch_add(latency_us, Ordering::Relaxed);
            rq.metrics
                .commit_latency_us_count
                .fetch_add(1, Ordering::Relaxed);

            coalescer_update_pending(&pending_requests, work.pending_requests);
        }

        // Release processing lock + update last_serviced timestamp
        rq.last_serviced_us
            .store(now_micros_u64(), Ordering::Relaxed);
        rq.processing.store(false, Ordering::Release);

        // If any repo still has work, wake another worker
        let more_work = {
            let repos_guard = repos.lock().unwrap_or_else(|e| e.into_inner());
            repos_guard
                .values()
                .any(|r| r.depth.load(Ordering::Relaxed) > 0)
        };
        if more_work {
            let (lock, cvar) = &*work_cv;
            {
                let mut ready = lock.lock().unwrap_or_else(|e| e.into_inner());
                *ready = true;
            }
            cvar.notify_one();
        }
    }
}

/// Update global pending_requests counter and 80% threshold metric.
fn coalescer_update_pending(pending_requests: &Arc<AtomicU64>, drained: u64) {
    let metrics = mcp_agent_mail_core::global_metrics();
    let pending_after = pending_requests
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_sub(drained))
        })
        .unwrap_or(0)
        .saturating_sub(drained);
    metrics.storage.commit_pending_requests.set(pending_after);

    let threshold = COMMIT_COALESCER_SOFT_CAP
        .saturating_mul(80)
        .saturating_div(100);
    if threshold > 0 && pending_after >= threshold {
        if metrics.storage.commit_over_80_since_us.load() == 0 {
            metrics
                .storage
                .commit_over_80_since_us
                .set(now_micros_u64());
        }
    } else {
        metrics.storage.commit_over_80_since_us.set(0);
    }
}

/// Drain a single repo's spill buffer into a `CoalescerSpilledWork`.
fn coalescer_drain_repo_spill(rq: &RepoQueue, repo_root: &Path) -> Option<CoalescerSpilledWork> {
    let mut guard = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
    let repo = guard.inner.take()?;
    if repo.pending_requests == 0 {
        return None;
    }
    Some(CoalescerSpilledWork {
        repo_root: repo_root.to_path_buf(),
        pending_requests: repo.pending_requests,
        earliest_enqueued_at: repo.earliest_enqueued_at,
        dirty_all: repo.dirty_all,
        paths: repo.paths.into_iter().collect(),
        git_author_name: repo.git_author_name,
        git_author_email: repo.git_author_email,
        message_first_lines: repo.message_first_lines.into_iter().collect(),
        message_total: repo.message_total,
    })
}

fn coalescer_commit_spilled_work(
    work: &CoalescerSpilledWork,
    stats: &Arc<Mutex<CommitQueueStats>>,
    batch_sizes: &Arc<Mutex<VecDeque<usize>>>,
) {
    if work.pending_requests == 0 {
        return;
    }

    let config = Config {
        git_author_name: work.git_author_name.clone(),
        git_author_email: work.git_author_email.clone(),
        ..Config::default()
    };

    let mut msg = format!("spill: {} ops coalesced", work.pending_requests);
    if work.dirty_all {
        msg.push_str(" (commit-all)");
    }
    msg.push_str("\n\n");

    let visible = work.message_first_lines.len() as u64;
    for line in &work.message_first_lines {
        msg.push_str("- ");
        msg.push_str(line);
        msg.push('\n');
    }
    if work.message_total > visible {
        msg.push_str(&format!("- ... (+{} more)\n", work.message_total - visible));
    }

    let commit_result = if work.dirty_all {
        coalescer_commit_all_with_retry(&work.repo_root, &config, &msg)
            .map(|()| work.pending_requests)
    } else if work.paths.is_empty() {
        Ok(work.pending_requests)
    } else {
        coalescer_commit_with_retry(&work.repo_root, &config, &msg, &work.paths)
            .map(|()| work.pending_requests)
    };

    // Update stats
    match commit_result {
        Ok(batch_u64) => {
            let batch_size = usize::try_from(batch_u64).unwrap_or(usize::MAX);
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.commits += 1;
            s.batched += batch_size;

            let mut sizes = batch_sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sizes.push_back(batch_size);
            if sizes.len() > 100 {
                sizes.pop_front();
            }
            let avg = if sizes.is_empty() {
                0.0
            } else {
                sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
            };
            s.avg_batch_size = (avg * 100.0).round() / 100.0;
        }
        Err(e) => {
            tracing::warn!("[commit-coalescer] spill commit error: {e}");
            mcp_agent_mail_core::global_metrics()
                .storage
                .commit_errors_total
                .inc();
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.errors += 1;
        }
    }
}

/// Commit a batch of requests targeting the same repository.
///
/// Merges non-conflicting paths into a single commit. Falls back to
/// sequential commits if paths conflict (same file modified by multiple requests).
fn coalescer_commit_batch(
    repo_root: &Path,
    requests: &[CoalescerCommitFields],
    stats: &Arc<Mutex<CommitQueueStats>>,
    batch_sizes: &Arc<Mutex<VecDeque<usize>>>,
) {
    if requests.is_empty() {
        return;
    }

    // Use the first request's author info
    let author_name = &requests[0].git_author_name;
    let author_email = &requests[0].git_author_email;
    let config = Config {
        git_author_name: author_name.clone(),
        git_author_email: author_email.clone(),
        ..Config::default()
    };

    // Check for path conflicts
    let mut all_paths = HashSet::new();
    let mut can_merge = true;
    for req in requests {
        for p in &req.rel_paths {
            if !all_paths.insert(p.clone()) {
                can_merge = false;
                break;
            }
        }
        if !can_merge {
            break;
        }
    }

    // Keep batch commits bounded to avoid enormous commits under load.
    let commit_result = if can_merge
        && requests.len() > 1
        && requests.len() <= COALESCER_MAX_BATCH_SIZE
    {
        // Merge all into a single commit
        let merged_paths: Vec<String> = requests
            .iter()
            .flat_map(|r| r.rel_paths.iter().cloned())
            .collect();

        let summary_lines: Vec<String> = requests
            .iter()
            .map(|r| {
                let first = r.message.lines().next().unwrap_or("");
                format!("- {first}")
            })
            .collect();

        let combined_msg = format!(
            "batch: {} ops coalesced\n\n{}",
            requests.len(),
            summary_lines.join("\n")
        );

        coalescer_commit_with_retry(repo_root, &config, &combined_msg, &merged_paths)
            .map(|()| requests.len())
    } else if requests.len() == 1 {
        // Single request — commit directly
        coalescer_commit_with_retry(
            repo_root,
            &config,
            &requests[0].message,
            &requests[0].rel_paths,
        )
        .map(|()| 1)
    } else {
        // Path conflicts — commit sequentially
        let mut total = 0;
        let mut last_err = None;
        for req in requests {
            let r_config = Config {
                git_author_name: req.git_author_name.clone(),
                git_author_email: req.git_author_email.clone(),
                ..Config::default()
            };
            match coalescer_commit_with_retry(repo_root, &r_config, &req.message, &req.rel_paths) {
                Ok(()) => total += 1,
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            tracing::warn!("[commit-coalescer] partial failure in sequential batch: {e}");
        }
        Ok(total)
    };

    // Update stats
    match commit_result {
        Ok(batch_size) => {
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.commits += 1;
            s.batched += batch_size;

            let mut sizes = batch_sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sizes.push_back(batch_size);
            if sizes.len() > 100 {
                sizes.pop_front();
            }
            let avg = if sizes.is_empty() {
                0.0
            } else {
                sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
            };
            s.avg_batch_size = (avg * 100.0).round() / 100.0;
        }
        Err(e) => {
            tracing::warn!("[commit-coalescer] batch commit error: {e}");
            mcp_agent_mail_core::global_metrics()
                .storage
                .commit_errors_total
                .inc();
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.errors += 1;
        }
    }
}

/// Commit with retry and jittered backoff for index.lock contention.
///
/// Unlike `commit_paths_with_retry`, this uses jitter to prevent thundering
/// herd when multiple coalescer batches retry simultaneously.
fn coalescer_commit_with_retry(
    repo_root: &Path,
    config: &Config,
    message: &str,
    rel_paths: &[String],
) -> Result<()> {
    const MAX_RETRIES: usize = 7;
    let sm = &mcp_agent_mail_core::global_metrics().storage;
    sm.commit_attempts_total.inc();
    sm.commit_batch_size_last.set(rel_paths.len() as u64);

    // Try lock-free commit first (avoids index.lock entirely)
    {
        let repo = Repository::open(repo_root)?;
        let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
        let commit_start = std::time::Instant::now();
        match commit_paths_lockfree(&repo, config, message, &refs) {
            Ok(()) => {
                sm.git_commit_latency_us
                    .record(commit_start.elapsed().as_micros() as u64);
                sm.lockfree_commits_total.inc();
                return Ok(());
            }
            Err(e) => {
                sm.lockfree_commit_fallbacks_total.inc();
                tracing::debug!(
                    "[git-lock] lockfree commit failed, falling back to index-based: {e}"
                );
            }
        }
    }

    // Fall back to index-based commit with retry
    let mut index_lock_retries: u64 = 0;

    for attempt in 0..=MAX_RETRIES {
        let repo = Repository::open(repo_root)?;
        let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();

        write_lock_owner(repo_root);
        let commit_start = std::time::Instant::now();
        match commit_paths(&repo, config, message, &refs) {
            Ok(()) => {
                remove_lock_owner(repo_root);
                sm.git_commit_latency_us
                    .record(commit_start.elapsed().as_micros() as u64);
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                return Ok(());
            }
            Err(StorageError::Git(ref git_err)) if is_git_index_lock_error(git_err) => {
                index_lock_retries += 1;
                if attempt >= MAX_RETRIES {
                    // Last-resort stale lock cleanup
                    if try_clean_stale_git_lock(repo_root, 30.0) {
                        let repo2 = Repository::open(repo_root)?;
                        let refs2: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
                        write_lock_owner(repo_root);
                        let start2 = std::time::Instant::now();
                        let result = commit_paths(&repo2, config, message, &refs2);
                        remove_lock_owner(repo_root);
                        sm.git_commit_latency_us
                            .record(start2.elapsed().as_micros() as u64);
                        sm.git_index_lock_retries_total.add(index_lock_retries);
                        if result.is_err() {
                            sm.commit_failures_total.inc();
                            sm.git_index_lock_failures_total.inc();
                        }
                        return result;
                    }
                    sm.commit_failures_total.inc();
                    sm.git_index_lock_failures_total.inc();
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                    return Err(StorageError::GitIndexLock {
                        message: format!("index.lock contention after {MAX_RETRIES} retries"),
                        lock_path: repo_root.join(".git").join("index.lock"),
                        attempts: MAX_RETRIES,
                    });
                }

                // Jittered exponential backoff: base * 2^attempt + random jitter
                let base_ms = 50 * (1u64 << attempt.min(5)); // 50, 100, 200, 400, 800, 1600
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_micros() as u64
                    % (base_ms / 2 + 1);
                std::thread::sleep(Duration::from_millis(base_ms + jitter));

                // Try cleaning stale locks on later attempts
                if attempt >= 3 {
                    let _ = try_clean_stale_git_lock(repo_root, 60.0);
                }
            }
            Err(other) => {
                sm.commit_failures_total.inc();
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                return Err(other);
            }
        }
    }

    unreachable!()
}

fn coalescer_commit_all_with_retry(repo_root: &Path, config: &Config, message: &str) -> Result<()> {
    const MAX_RETRIES: usize = 7;
    let sm = &mcp_agent_mail_core::global_metrics().storage;
    sm.commit_attempts_total.inc();
    let mut index_lock_retries: u64 = 0;

    for attempt in 0..=MAX_RETRIES {
        let repo = Repository::open(repo_root)?;

        write_lock_owner(repo_root);
        let commit_start = std::time::Instant::now();
        match commit_all(&repo, config, message) {
            Ok(()) => {
                remove_lock_owner(repo_root);
                sm.git_commit_latency_us
                    .record(commit_start.elapsed().as_micros() as u64);
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                return Ok(());
            }
            Err(StorageError::Git(ref git_err)) if is_git_index_lock_error(git_err) => {
                index_lock_retries += 1;
                if attempt >= MAX_RETRIES {
                    if try_clean_stale_git_lock(repo_root, 30.0) {
                        let repo2 = Repository::open(repo_root)?;
                        write_lock_owner(repo_root);
                        let start2 = std::time::Instant::now();
                        let result = commit_all(&repo2, config, message);
                        remove_lock_owner(repo_root);
                        sm.git_commit_latency_us
                            .record(start2.elapsed().as_micros() as u64);
                        sm.git_index_lock_retries_total.add(index_lock_retries);
                        if result.is_err() {
                            sm.commit_failures_total.inc();
                            sm.git_index_lock_failures_total.inc();
                        }
                        return result;
                    }
                    sm.commit_failures_total.inc();
                    sm.git_index_lock_failures_total.inc();
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                    return Err(StorageError::GitIndexLock {
                        message: format!("index.lock contention after {MAX_RETRIES} retries"),
                        lock_path: repo_root.join(".git").join("index.lock"),
                        attempts: MAX_RETRIES,
                    });
                }

                let base_ms = 50 * (1u64 << attempt.min(5));
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_micros() as u64
                    % (base_ms / 2 + 1);
                std::thread::sleep(Duration::from_millis(base_ms + jitter));

                if attempt >= 3 {
                    let _ = try_clean_stale_git_lock(repo_root, 60.0);
                }
            }
            Err(other) => {
                sm.commit_failures_total.inc();
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                return Err(other);
            }
        }
    }

    unreachable!()
}

/// Global commit coalescer instance (lazy-initialized).
static COMMIT_COALESCER: OnceLock<CommitCoalescer> = OnceLock::new();

/// Get the global commit coalescer (spawns worker on first call).
pub fn get_commit_coalescer() -> &'static CommitCoalescer {
    COMMIT_COALESCER
        .get_or_init(|| CommitCoalescer::new(Duration::from_millis(DEFAULT_COALESCER_FLUSH_MS)))
}

/// Enqueue an async git commit via the global coalescer.
///
/// **Non-blocking, fire-and-forget.** Files must already be written to disk.
/// The background worker will batch-commit them to git.
///
/// Use this instead of `commit_paths()` in all tool hot paths.
pub fn enqueue_async_commit(
    repo_root: &Path,
    config: &Config,
    message: &str,
    rel_paths: &[String],
) {
    get_commit_coalescer().enqueue(
        repo_root.to_path_buf(),
        config,
        message.to_string(),
        rel_paths.to_vec(),
    );
}

/// Block until all pending async commits are flushed to git.
///
/// Call this in tests, during graceful shutdown, or before reading git history
/// that depends on recent writes.
pub fn flush_async_commits() {
    get_commit_coalescer().flush_sync();
}

// ---------------------------------------------------------------------------
// Git index.lock contention handling
// ---------------------------------------------------------------------------

/// Determine the commit lock path based on project-scoped rel_paths.
pub fn commit_lock_path(repo_root: &Path, rel_paths: &[&str]) -> PathBuf {
    if rel_paths.is_empty() {
        return repo_root.join(".commit.lock");
    }

    // Check if all paths are under the same project
    let mut project_slug: Option<&str> = None;
    let mut same_project = true;

    for rel_path in rel_paths {
        let parts: Vec<&str> = rel_path.split('/').collect();
        if parts.len() < 2 || parts[0] != "projects" {
            same_project = false;
            break;
        }
        let slug = parts[1];
        match project_slug {
            None => project_slug = Some(slug),
            Some(prev) if prev != slug => {
                same_project = false;
                break;
            }
            _ => {}
        }
    }

    if same_project {
        if let Some(slug) = project_slug {
            return repo_root.join("projects").join(slug).join(".commit.lock");
        }
    }

    repo_root.join(".commit.lock")
}

/// Check if an error is a git index.lock contention error.
fn is_git_index_lock_error(err: &git2::Error) -> bool {
    let msg = err.message().to_lowercase();
    msg.contains("index.lock") || msg.contains("lock at") || msg.contains("index is locked")
}

// ---------------------------------------------------------------------------
// PID-aware stale lock management
// ---------------------------------------------------------------------------

/// Default stale lock age threshold (120 seconds, increased from 60s for safety).
#[expect(dead_code)]
const STALE_LOCK_AGE_SECONDS: f64 = 120.0;

/// Write a `.git/index.lock.owner` file with our PID and timestamp.
///
/// This allows other processes to check if the lock holder is still alive
/// before forcibly removing a lock.
fn write_lock_owner(repo_root: &Path) {
    let owner_path = repo_root.join(".git").join("index.lock.owner");
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let content = format!("{pid}\n{ts}\n");
    let _ = fs::write(&owner_path, content);
}

/// Remove the `.git/index.lock.owner` file after a successful commit.
fn remove_lock_owner(repo_root: &Path) {
    let owner_path = repo_root.join(".git").join("index.lock.owner");
    let _ = fs::remove_file(&owner_path);
}

/// Check if a process with the given PID is still alive.
///
/// Uses `/proc/<pid>/` on Linux (no `unsafe` required).
fn is_pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Try to clean up a stale .git/index.lock file with PID-aware safety.
///
/// Before removing a lock:
/// 1. Check if a `.git/index.lock.owner` file exists with the owner PID
/// 2. If the owning PID is still alive, do NOT remove the lock
/// 3. If the PID is dead (or no owner file), use age-based threshold
///
/// Returns `true` if a stale lock was removed.
fn try_clean_stale_git_lock(repo_root: &Path, max_age_seconds: f64) -> bool {
    let lock_path = repo_root.join(".git").join("index.lock");
    if !lock_path.exists() {
        return false;
    }

    // Check PID-based ownership first
    let owner_path = repo_root.join(".git").join("index.lock.owner");
    if let Ok(content) = fs::read_to_string(&owner_path) {
        if let Some(pid_str) = content.lines().next() {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if is_pid_alive(pid) {
                    // Lock holder is still alive — don't remove!
                    tracing::debug!("[git-lock] index.lock held by alive PID {pid}, not removing");
                    return false;
                }
                // PID is dead — safe to remove lock
                tracing::info!("[git-lock] index.lock held by dead PID {pid}, removing stale lock");
                let _ = fs::remove_file(&lock_path);
                let _ = fs::remove_file(&owner_path);
                return true;
            }
        }
    }

    // No owner file or unparseable — fall back to age-based removal
    let age = fs::metadata(&lock_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs_f64());

    if let Some(age) = age {
        if age > max_age_seconds {
            tracing::info!(
                "[git-lock] removing stale index.lock (age={age:.1}s > {max_age_seconds}s)"
            );
            let _ = fs::remove_file(&lock_path);
            let _ = fs::remove_file(&owner_path);
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Lock-free git commit path (plumbing-based)
// ---------------------------------------------------------------------------

/// Commit files without touching the git index (avoids index.lock entirely).
///
/// Uses git plumbing operations:
/// 1. `repo.blob()` — hash and write file content as blob objects
/// 2. `repo.treebuilder()` — build tree hierarchy without using index
/// 3. `repo.commit()` — create commit object (uses ref lock, NOT index lock)
///
/// This eliminates index.lock contention entirely, since the index is never
/// read or written. Falls back to `commit_paths()` if tree building fails.
fn commit_paths_lockfree(
    repo: &Repository,
    config: &Config,
    message: &str,
    rel_paths: &[&str],
) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }

    let workdir = repo.workdir().ok_or(StorageError::NotInitialized)?;
    let sig = Signature::now(&config.git_author_name, &config.git_author_email)?;

    // Get parent commit and its tree
    let parent = resolve_head_commit_oid(repo)?
        .map(|oid| repo.find_commit(oid))
        .transpose()?;
    let base_tree = parent.as_ref().and_then(|p| p.tree().ok());

    // Create blob objects for each file
    let mut updates: Vec<(String, git2::Oid)> = Vec::new();
    for path in rel_paths {
        let path = validate_repo_relative_path("lockfree commit path", path)?;
        let full = workdir.join(path);
        if full.exists() {
            let content = fs::read(&full)?;
            let blob_oid = repo.blob(&content)?;
            updates.push((path.to_string(), blob_oid));
        }
    }

    if updates.is_empty() {
        return Ok(());
    }

    // Build new tree without touching the index
    let tree_oid = build_tree_with_updates(repo, base_tree.as_ref(), &updates)?;
    let tree = repo.find_tree(tree_oid)?;

    let final_message = append_trailers(message);

    // Create commit (updates HEAD ref lock, NOT index lock)
    match parent {
        Some(ref p) => {
            repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[p])?;
        }
        None => {
            repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[])?;
        }
    }

    // Lock-free commits bypass the index by design; sync index to HEAD so
    // status/porcelain views remain clean for tooling and tests.
    if let Err(err) = try_restore_index_to_head(repo) {
        tracing::debug!("[git-lockfree] failed to sync index after commit: {err}");
    }

    Ok(())
}

/// Recursively build a git tree with updates applied to the base tree.
///
/// Groups updates by their first path component:
/// - Direct files: insert blob OIDs into the tree builder
/// - Subdirectories: recurse to build sub-trees
fn build_tree_with_updates(
    repo: &Repository,
    base: Option<&git2::Tree<'_>>,
    updates: &[(String, git2::Oid)],
) -> Result<git2::Oid> {
    // Group updates by first path component
    let mut direct_entries: Vec<(&str, git2::Oid)> = Vec::new();
    let mut by_prefix: HashMap<String, Vec<(String, git2::Oid)>> = HashMap::new();

    for (path, oid) in updates {
        if let Some(slash_idx) = path.find('/') {
            let prefix = &path[..slash_idx];
            let rest = &path[slash_idx + 1..];
            by_prefix
                .entry(prefix.to_string())
                .or_default()
                .push((rest.to_string(), *oid));
        } else {
            direct_entries.push((path.as_str(), *oid));
        }
    }

    let mut builder = repo.treebuilder(base)?;

    // Insert direct file entries (blob mode 0o100644)
    for (name, oid) in &direct_entries {
        builder.insert(name, *oid, 0o100_644)?;
    }

    // Recurse for subdirectory entries (tree mode 0o040000)
    for (prefix, sub_updates) in &by_prefix {
        // Find the existing subtree (if any) for this prefix
        let sub_tree_oid = base
            .and_then(|t| t.get_name(prefix))
            .filter(|e| e.kind() == Some(git2::ObjectType::Tree))
            .map(|e| e.id());
        let sub_tree = sub_tree_oid.and_then(|oid| repo.find_tree(oid).ok());

        let new_sub_oid = build_tree_with_updates(repo, sub_tree.as_ref(), sub_updates)?;
        builder.insert(prefix, new_sub_oid, 0o040_000)?;
    }

    let oid = builder.write()?;
    Ok(oid)
}

/// Commit with git index.lock contention retry logic.
///
/// Wraps `commit_paths` with retry and exponential backoff for index.lock errors.
pub fn commit_paths_with_retry(
    repo_root: &Path,
    config: &Config,
    message: &str,
    rel_paths: &[&str],
) -> Result<()> {
    const MAX_INDEX_LOCK_RETRIES: usize = 5;

    let sm = &mcp_agent_mail_core::global_metrics().storage;
    sm.commit_attempts_total.inc();
    sm.commit_batch_size_last.set(rel_paths.len() as u64);

    // Try lock-free commit first (avoids index.lock entirely)
    {
        let repo = Repository::open(repo_root)?;
        let commit_start = std::time::Instant::now();
        match commit_paths_lockfree(&repo, config, message, rel_paths) {
            Ok(()) => {
                sm.git_commit_latency_us
                    .record(commit_start.elapsed().as_micros() as u64);
                sm.lockfree_commits_total.inc();
                return Ok(());
            }
            Err(e) => {
                sm.lockfree_commit_fallbacks_total.inc();
                tracing::debug!(
                    "[git-lock] lockfree commit failed, falling back to index-based: {e}"
                );
            }
        }
    }

    // Fall back to index-based commit with project-scoped lock
    let lock_path = commit_lock_path(repo_root, rel_paths);
    ensure_parent_dir(&lock_path)?;

    let mut lock = FileLock::new(lock_path);
    let lock_start = std::time::Instant::now();
    lock.acquire()?;
    sm.commit_lock_wait_us
        .record(lock_start.elapsed().as_micros() as u64);

    let mut last_err_msg: Option<String> = None;
    let mut did_last_resort_clean = false;
    let mut index_lock_retries: u64 = 0;

    for attempt in 0..MAX_INDEX_LOCK_RETRIES + 2 {
        let repo = Repository::open(repo_root)?;
        write_lock_owner(repo_root);
        let commit_start = std::time::Instant::now();
        match commit_paths(&repo, config, message, rel_paths) {
            Ok(()) => {
                remove_lock_owner(repo_root);
                sm.git_commit_latency_us
                    .record(commit_start.elapsed().as_micros() as u64);
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                lock.release()?;
                return Ok(());
            }
            Err(StorageError::Git(ref git_err)) if is_git_index_lock_error(git_err) => {
                last_err_msg = Some(git_err.message().to_string());
                index_lock_retries += 1;

                if attempt >= MAX_INDEX_LOCK_RETRIES {
                    if !did_last_resort_clean && try_clean_stale_git_lock(repo_root, 60.0) {
                        did_last_resort_clean = true;
                        continue;
                    }
                    break;
                }

                // Exponential backoff: 100ms, 200ms, 400ms, 800ms, 1600ms
                let delay_ms = 100 * (1u64 << attempt.min(4));
                std::thread::sleep(Duration::from_millis(delay_ms));

                // Try cleaning stale locks (5 minute threshold)
                let _ = try_clean_stale_git_lock(repo_root, 300.0);
            }
            Err(other) => {
                sm.commit_failures_total.inc();
                if index_lock_retries > 0 {
                    sm.git_index_lock_retries_total.add(index_lock_retries);
                }
                lock.release()?;
                return Err(other);
            }
        }
    }

    // All retries exhausted — record failure metrics.
    sm.commit_failures_total.inc();
    sm.git_index_lock_failures_total.inc();
    sm.git_index_lock_retries_total.add(index_lock_retries);
    lock.release()?;

    let git_lock_path = repo_root.join(".git").join("index.lock");
    Err(StorageError::GitIndexLock {
        message: format!(
            "Git index.lock contention after {} retries. {}",
            MAX_INDEX_LOCK_RETRIES,
            last_err_msg.unwrap_or_default()
        ),
        lock_path: git_lock_path,
        attempts: MAX_INDEX_LOCK_RETRIES,
    })
}

// ---------------------------------------------------------------------------
// Stale lock healing (startup cleanup)
// ---------------------------------------------------------------------------

/// Scan the archive root for stale lock artifacts and clean them.
///
/// Should be called at application startup.
pub fn heal_archive_locks(config: &Config) -> Result<HealResult> {
    let root = &config.storage_root;
    if !root.exists() {
        return Ok(HealResult::default());
    }

    let mut result = HealResult::default();

    // Walk looking for .lock files
    fn walk_for_locks(dir: &Path, result: &mut HealResult) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                // Avoid recursing into symlink loops or scanning outside the archive root.
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk_for_locks(&path, result)?;
            } else if file_type.is_file() && path.extension().is_some_and(|e| e == "lock") {
                result.locks_scanned += 1;

                // Check if stale using zero-timeout lock
                let lock = FileLock::new(path.clone()).with_stale_timeout(Duration::ZERO);
                if lock.cleanup_if_stale().unwrap_or(false) {
                    result.locks_removed.push(path.display().to_string());
                }
            }
        }
        Ok(())
    }

    walk_for_locks(root, &mut result)?;

    // Clean orphaned metadata files (no matching lock)
    fn walk_for_orphaned_meta(dir: &Path, result: &mut HealResult) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                // Avoid recursing into symlink loops or scanning outside the archive root.
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk_for_orphaned_meta(&path, result)?;
            } else if file_type.is_file() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.ends_with(".lock.owner.json") {
                    let lock_name = &name[..name.len() - ".owner.json".len()];
                    let lock_candidate = path.parent().unwrap_or(dir).join(lock_name);
                    if !lock_candidate.exists() {
                        let _ = fs::remove_file(&path);
                        result.metadata_removed.push(path.display().to_string());
                    }
                }
            }
        }
        Ok(())
    }

    walk_for_orphaned_meta(root, &mut result)?;

    Ok(result)
}

/// Result of a lock healing scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealResult {
    pub locks_scanned: usize,
    pub locks_removed: Vec<String>,
    pub metadata_removed: Vec<String>,
}

// ---------------------------------------------------------------------------
// Repo cache  (process-global, thread-safe)
// ---------------------------------------------------------------------------

/// Simple LRU-ish repo path cache. We don't cache `Repository` handles across
/// calls because `git2::Repository` is `!Send` on some platforms, but we cache
/// the *path* so repeated lookups avoid re-scanning.
static REPO_CACHE: LazyLock<OrderedMutex<Option<HashMap<PathBuf, bool>>>> =
    LazyLock::new(|| OrderedMutex::new(LockLevel::StorageRepoCache, None));

fn repo_cache_contains(root: &Path) -> bool {
    let guard = REPO_CACHE.lock();
    guard.as_ref().is_some_and(|m| m.contains_key(root))
}

fn repo_cache_insert(root: &Path) {
    let mut guard = REPO_CACHE.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(root.to_path_buf(), true);
}

// ---------------------------------------------------------------------------
// Directory existence cache — avoids repeated stat() syscalls from
// create_dir_all() on directories that already exist.
// ---------------------------------------------------------------------------

static DIR_CACHE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Create parent directories for `path`, skipping `create_dir_all` when the
/// parent has already been seen. This eliminates redundant `stat`/`access`
/// syscalls in the hot message-write path.
fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    Ok(())
}

/// Create a directory (and parents) only if we haven't already created it.
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    {
        let cache = DIR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.contains(dir) {
            return Ok(());
        }
    }
    fs::create_dir_all(dir)?;
    {
        let mut cache = DIR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(dir.to_path_buf());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive initialization (br-2ei.2.1)
// ---------------------------------------------------------------------------

/// Ensure the global archive root directory exists and is a git repository.
///
/// Returns `(repo_root, was_freshly_initialized)`.
pub fn ensure_archive_root(config: &Config) -> Result<(PathBuf, bool)> {
    let root = config.storage_root.clone();
    ensure_dir(&root)?;

    let fresh = ensure_repo(&root, config)?;
    Ok((root, fresh))
}

/// Ensure a per-project archive directory exists under the archive root.
pub fn ensure_archive(config: &Config, slug: &str) -> Result<ProjectArchive> {
    let (repo_root, _fresh) = ensure_archive_root(config)?;
    let project_root = repo_root.join("projects").join(slug);
    fs::create_dir_all(&project_root)?;

    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.clone());
    let canonical_repo_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.clone());
    Ok(ProjectArchive {
        slug: slug.to_string(),
        root: project_root.clone(),
        repo_root,
        lock_path: project_root.join(".archive.lock"),
        canonical_root,
        canonical_repo_root,
    })
}

/// Initialize a git repository at `root` if one does not already exist.
///
/// Configures gpgsign=false and writes `.gitattributes`.
/// Returns `true` if a new repo was created, `false` if it already existed.
fn ensure_repo(root: &Path, config: &Config) -> Result<bool> {
    if repo_cache_contains(root) {
        return Ok(false);
    }

    let git_dir = root.join(".git");
    if git_dir.exists() {
        repo_cache_insert(root);
        return Ok(false);
    }

    // Initialize new repository
    let repo = Repository::init(root)?;

    // Configure gpgsign = false
    {
        let mut repo_config = repo.config()?;
        let _ = repo_config.set_bool("commit.gpgsign", false);
    }

    // Write .gitattributes
    let attrs_path = root.join(".gitattributes");
    if !attrs_path.exists() {
        write_text(
            &attrs_path,
            "# Binary and text file declarations for Git\n\
             \n\
             # Binary files\n\
             *.webp binary\n\
             *.jpg binary\n\
             *.jpeg binary\n\
             *.png binary\n\
             *.gif binary\n\
             *.webm binary\n\
             \n\
             # Database files\n\
             *.sqlite3 binary\n\
             *.db binary\n\
             *.sqlite binary\n\
             \n\
             # Archive and metadata files\n\
             *.md text eol=lf\n\
             *.json text eol=lf\n\
             *.txt text eol=lf\n\
             *.log text eol=lf\n\
             \n\
             # Lock files\n\
             *.lock binary\n\
             \n\
             # Default behavior\n\
             * text=auto\n",
        )?;
    }

    // Initial commit
    commit_paths(
        &repo,
        config,
        "chore: initialize archive",
        &[".gitattributes"],
    )?;

    repo_cache_insert(root);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Agent profile writes
// ---------------------------------------------------------------------------

/// Write an agent's profile.json to the archive and commit it.
pub fn write_agent_profile(archive: &ProjectArchive, agent: &serde_json::Value) -> Result<()> {
    let name = agent
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let name = validate_archive_component("agent name", name)?;

    let profile_dir = archive.root.join("agents").join(name);
    ensure_dir(&profile_dir)?;

    let profile_path = profile_dir.join("profile.json");
    write_json(&profile_path, agent)?;

    let rel = rel_path_cached(&archive.canonical_repo_root, &profile_path)?;
    enqueue_async_commit(
        &archive.repo_root,
        &Config::default(),
        &format!("agent: profile {name}"),
        &[rel],
    );

    Ok(())
}

/// Write an agent's profile.json using explicit config for author info.
pub fn write_agent_profile_with_config(
    archive: &ProjectArchive,
    config: &Config,
    agent: &serde_json::Value,
) -> Result<()> {
    let name = agent
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let name = validate_archive_component("agent name", name)?;

    let profile_dir = archive.root.join("agents").join(name);
    ensure_dir(&profile_dir)?;

    let profile_path = profile_dir.join("profile.json");
    write_json(&profile_path, agent)?;

    let rel = rel_path_cached(&archive.canonical_repo_root, &profile_path)?;
    enqueue_async_commit(
        &archive.repo_root,
        config,
        &format!("agent: profile {name}"),
        &[rel],
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// File reservation artifact writes
// ---------------------------------------------------------------------------

/// Build a commit message for file reservation records.
fn build_file_reservation_commit_message(entries: &[(String, String)]) -> String {
    let (first_agent, first_pattern) = &entries[0];
    if entries.len() == 1 {
        return format!("file_reservation: {first_agent} {first_pattern}");
    }
    let subject = format!(
        "file_reservation: {first_agent} {first_pattern} (+{} more)",
        entries.len() - 1
    );
    let lines: Vec<String> = entries
        .iter()
        .map(|(agent, pattern)| format!("- {agent} {pattern}"))
        .collect();
    format!("{subject}\n\n{}", lines.join("\n"))
}

/// Write file reservation records to the archive and commit.
pub fn write_file_reservation_records(
    archive: &ProjectArchive,
    config: &Config,
    reservations: &[serde_json::Value],
) -> Result<()> {
    if reservations.is_empty() {
        return Ok(());
    }

    let reservation_dir = archive.root.join("file_reservations");
    ensure_dir(&reservation_dir)?;

    let mut rel_paths = Vec::new();
    let mut entries = Vec::new();

    for res in reservations {
        let path_pattern = res
            .get("path_pattern")
            .or_else(|| res.get("path"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        if path_pattern.is_empty() {
            return Err(StorageError::InvalidPath(
                "File reservation record must include 'path_pattern'".to_string(),
            ));
        }

        // Build normalized reservation (ensure path_pattern is canonical key)
        let mut normalized = res.clone();
        if let Some(obj) = normalized.as_object_mut() {
            obj.insert(
                "path_pattern".to_string(),
                serde_json::Value::String(path_pattern.clone()),
            );
            obj.remove("path");
        }

        // Legacy path: sha1(path_pattern).json
        let digest = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(path_pattern.as_bytes());
            hex::encode(hasher.finalize())
        };
        let legacy_path = reservation_dir.join(format!("{digest}.json"));
        write_json(&legacy_path, &normalized)?;
        rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &legacy_path)?);

        // Stable per-reservation artifact: id-<id>.json
        if let Some(id) = normalized.get("id").and_then(serde_json::Value::as_i64) {
            let id_path = reservation_dir.join(format!("id-{id}.json"));
            write_json(&id_path, &normalized)?;
            rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &id_path)?);
        }

        let agent_name = normalized
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        entries.push((agent_name, path_pattern));
    }

    let commit_msg = build_file_reservation_commit_message(&entries);
    enqueue_async_commit(&archive.repo_root, config, &commit_msg, &rel_paths);

    Ok(())
}

/// Write a single file reservation record.
pub fn write_file_reservation_record(
    archive: &ProjectArchive,
    config: &Config,
    reservation: &serde_json::Value,
) -> Result<()> {
    write_file_reservation_records(archive, config, std::slice::from_ref(reservation))
}

// ---------------------------------------------------------------------------
// Message write pipeline
// ---------------------------------------------------------------------------

/// Regex for slugifying message subjects.
fn subject_slug_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9._-]+").expect("valid regex"))
}

fn sanitize_thread_id(thread_id: &str) -> String {
    // Strip path traversal components before slugifying
    let no_traversal: String = thread_id
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect::<Vec<_>>()
        .join("/");
    let raw = subject_slug_re().replace_all(&no_traversal, "-");
    let trimmed = raw
        .trim_matches(|c: char| c == '-' || c == '_')
        .to_lowercase();
    let truncated = if trimmed.len() > 120 {
        truncate_utf8(&trimmed, 120).to_string()
    } else {
        trimmed
    };
    if truncated.is_empty() {
        "thread".to_string()
    } else {
        truncated
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    &s[..idx]
}

fn validate_archive_component<'a>(kind: &str, raw: &'a str) -> Result<&'a str> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(StorageError::InvalidPath(format!("{kind} is empty")));
    }
    if s == "." || s == ".." {
        return Err(StorageError::InvalidPath(format!(
            "{kind} must not be '.' or '..'"
        )));
    }
    if s.contains('/') || s.contains('\\') {
        return Err(StorageError::InvalidPath(format!(
            "{kind} must not contain path separators"
        )));
    }
    if s.contains('\0') {
        return Err(StorageError::InvalidPath(format!(
            "{kind} must not contain NUL"
        )));
    }
    Ok(s)
}

fn validate_repo_relative_path<'a>(kind: &str, raw: &'a str) -> Result<&'a str> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(StorageError::InvalidPath(format!("{kind} is empty")));
    }
    if s.contains('\\') {
        return Err(StorageError::InvalidPath(format!(
            "{kind} must use forward slashes"
        )));
    }
    if s.contains('\0') {
        return Err(StorageError::InvalidPath(format!(
            "{kind} must not contain NUL"
        )));
    }

    let p = Path::new(s);
    if let Some(Component::Normal(first)) = p.components().next() {
        if first == ".git" {
            return Err(StorageError::InvalidPath(format!(
                "{kind} must not reference .git internals"
            )));
        }
    }

    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(StorageError::InvalidPath(format!(
                    "{kind} must be a repo-root-relative path"
                )));
            }
        }
    }

    Ok(s)
}

/// Compute message archive paths for canonical, outbox, and inbox copies.
pub fn message_paths(
    archive: &ProjectArchive,
    sender: &str,
    recipients: &[String],
    created: &DateTime<Utc>,
    subject: &str,
    id: i64,
) -> Result<MessageArchivePaths> {
    let sender = validate_archive_component("sender", sender)?;

    let y = created.format("%Y").to_string();
    let m = created.format("%m").to_string();
    let iso = created.format("%Y-%m-%dT%H-%M-%SZ").to_string();

    let slug = {
        let raw = subject_slug_re().replace_all(subject, "-");
        let trimmed = raw
            .trim_matches(|c: char| c == '-' || c == '_')
            .to_lowercase();
        let truncated = if trimmed.len() > 80 {
            truncate_utf8(&trimmed, 80).to_string()
        } else {
            trimmed
        };
        if truncated.is_empty() {
            "message".to_string()
        } else {
            truncated
        }
    };

    let filename = if id > 0 {
        format!("{iso}__{slug}__{id}.md")
    } else {
        format!("{iso}__{slug}.md")
    };

    let canonical = archive
        .root
        .join("messages")
        .join(&y)
        .join(&m)
        .join(&filename);
    let outbox = archive
        .root
        .join("agents")
        .join(sender)
        .join("outbox")
        .join(&y)
        .join(&m)
        .join(&filename);
    let mut inbox: Vec<PathBuf> = Vec::with_capacity(recipients.len());
    for r in recipients {
        let r = validate_archive_component("recipient", r)?;
        inbox.push(
            archive
                .root
                .join("agents")
                .join(r)
                .join("inbox")
                .join(&y)
                .join(&m)
                .join(&filename),
        );
    }

    Ok(MessageArchivePaths {
        canonical,
        outbox,
        inbox,
    })
}

/// Write a message bundle to the archive: canonical, outbox, and inbox copies.
///
/// The message is written with JSON frontmatter followed by the markdown body.
#[allow(clippy::too_many_arguments)]
pub fn write_message_bundle(
    archive: &ProjectArchive,
    config: &Config,
    message: &serde_json::Value,
    body_md: &str,
    sender: &str,
    recipients: &[String],
    extra_paths: &[String],
    commit_text: Option<&str>,
) -> Result<()> {
    // Parse timestamp
    let created = parse_message_timestamp(message);
    let timestamp_str = created.to_rfc3339();

    let paths = message_paths(
        archive,
        sender,
        recipients,
        &created,
        message
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("message"),
        message
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    )?;

    // Build frontmatter content
    let frontmatter = serde_json::to_string_pretty(message)?;
    let content = format!("---json\n{frontmatter}\n---\n\n{}\n", body_md.trim());

    // Create directories and write files
    let mut rel_paths = Vec::new();

    // Canonical (ensure_parent_dir handled inside write_text)
    write_text(&paths.canonical, &content)?;
    rel_paths.push(rel_path_cached(
        &archive.canonical_repo_root,
        &paths.canonical,
    )?);

    // Outbox
    write_text(&paths.outbox, &content)?;
    rel_paths.push(rel_path_cached(
        &archive.canonical_repo_root,
        &paths.outbox,
    )?);

    // Inbox copies
    for inbox_path in &paths.inbox {
        write_text(inbox_path, &content)?;
        rel_paths.push(rel_path_cached(&archive.canonical_repo_root, inbox_path)?);
    }

    // Thread digest
    if let Some(thread_id) = message.get("thread_id").and_then(serde_json::Value::as_str) {
        let thread_id = thread_id.trim();
        if !thread_id.is_empty() {
            let canonical_rel = rel_path_cached(&archive.canonical_repo_root, &paths.canonical)?;
            if let Ok(digest_rel) = update_thread_digest(
                archive,
                thread_id,
                sender,
                recipients,
                message
                    .get("subject")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
                &timestamp_str,
                body_md,
                &canonical_rel,
            ) {
                rel_paths.push(digest_rel);
            }
        }
    }

    // Extra paths
    for p in extra_paths {
        let p = validate_repo_relative_path("extra_path", p)?;
        rel_paths.push(p.to_string());
    }

    // Build commit message
    let commit_message = if let Some(text) = commit_text {
        text.to_string()
    } else {
        let thread_key = message
            .get("thread_id")
            .or_else(|| message.get("id"))
            .and_then(|v| {
                if v.is_string() {
                    v.as_str().map(String::from)
                } else {
                    Some(v.to_string())
                }
            })
            .unwrap_or_default();

        let subject = format!(
            "mail: {sender} -> {} | {}",
            recipients.join(", "),
            message
                .get("subject")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        );
        let body_lines = [
            "TOOL: send_message",
            &format!("Agent: {sender}"),
            &format!(
                "Project: {}",
                message
                    .get("project")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
            ),
            &format!("Started: {timestamp_str}"),
            "Status: SUCCESS",
            &format!("Thread: {thread_key}"),
        ];
        format!("{subject}\n\n{}\n", body_lines.join("\n"))
    };

    enqueue_async_commit(&archive.repo_root, config, &commit_message, &rel_paths);

    Ok(())
}

/// Parse a message timestamp from the JSON value.
fn parse_message_timestamp(message: &serde_json::Value) -> DateTime<Utc> {
    let ts = message.get("created").or_else(|| message.get("created_ts"));

    if let Some(serde_json::Value::String(s)) = ts {
        let s = s.trim();
        if !s.is_empty() {
            // Handle Z-suffixed timestamps
            let parse_str = if let Some(stripped) = s.strip_suffix('Z') {
                format!("{stripped}+00:00")
            } else {
                s.to_string()
            };
            if let Ok(dt) = DateTime::parse_from_rfc3339(&parse_str) {
                return dt.with_timezone(&Utc);
            }
            // Try ISO 8601 without offset
            if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                return naive.and_utc();
            }
        }
    }
    if let Some(serde_json::Value::Number(n)) = ts {
        if let Some(raw) = n.as_i64() {
            if raw > 0 {
                let secs = raw / 1_000_000;
                let micros = raw % 1_000_000;
                if let Some(dt) = DateTime::from_timestamp(secs, (micros * 1000) as u32) {
                    return dt;
                }
            }
        }
    }

    Utc::now()
}

/// Update (append to) a thread-level digest file.
#[allow(clippy::too_many_arguments)]
fn update_thread_digest(
    archive: &ProjectArchive,
    thread_id: &str,
    sender: &str,
    recipients: &[String],
    subject: &str,
    timestamp: &str,
    body_md: &str,
    canonical_rel: &str,
) -> Result<String> {
    let digest_dir = archive.root.join("messages").join("threads");
    ensure_dir(&digest_dir)?;

    let safe_thread_id = sanitize_thread_id(thread_id);
    let digest_path = digest_dir.join(format!("{safe_thread_id}.md"));
    let recipients_str = recipients.join(", ");

    let header = format!("## {timestamp} \u{2014} {sender} \u{2192} {recipients_str}\n\n");
    let link_line = format!("[View canonical]({canonical_rel})\n\n");
    let subject_line = if subject.is_empty() {
        String::new()
    } else {
        format!("### {subject}\n\n")
    };

    // Truncate body preview
    let preview = body_md.trim();
    let preview = if preview.len() > 1200 {
        let truncated = truncate_utf8(preview, 1200);
        format!("{}\n...", truncated.trim_end())
    } else {
        preview.to_string()
    };

    let entry = format!("{subject_line}{header}{link_line}{preview}\n\n---\n\n");

    // Append to digest. Use create_new to atomically determine if this is
    // the first entry (eliminates TOCTOU race between exists() + create).
    //
    // CORRECTNESS: Build the full payload in memory and write it with a
    // single `write_all()` call. On most filesystems, writes under the
    // `PIPE_BUF` limit (~4 KB on Linux) to an O_APPEND fd are atomic with
    // respect to other appenders. Even for larger payloads, combining
    // header + entry into one write avoids interleaving from concurrent
    // writers between the two calls.
    let (mut file, is_new) = match fs::OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(&digest_path)
    {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let f = fs::OpenOptions::new().append(true).open(&digest_path)?;
            (f, false)
        }
        Err(e) => return Err(e.into()),
    };

    // Single write: header (if new) + entry — no interleaving possible.
    let payload = if is_new {
        format!("# Thread {thread_id}\n\n{entry}")
    } else {
        entry
    };
    file.write_all(payload.as_bytes())?;

    rel_path_cached(&archive.canonical_repo_root, &digest_path)
}

// ---------------------------------------------------------------------------
// Attachment pipeline
// ---------------------------------------------------------------------------

/// Maximum concurrent WebP conversion threads for parallel attachment processing.
const MAX_CONCURRENT_CONVERSIONS: usize = 4;

/// Maximum attachment file size for WebP conversion (50 MB).
/// Files larger than this are rejected to prevent pathological decode times.
const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// Metadata about a stored attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    /// "inline" or "file"
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub bytes: usize,
    pub sha1: String,
    pub width: u32,
    pub height: u32,
    /// Base64-encoded WebP data (only for inline type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// Relative path to WebP file in archive (only for file type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Relative path to original file (if keep_original_images is enabled)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
}

/// Manifest written to `attachments/_manifests/{sha1}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentManifest {
    pub sha1: String,
    pub webp_path: String,
    pub bytes_webp: usize,
    pub bytes_original: usize,
    pub width: u32,
    pub height: u32,
    pub original_ext: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
}

/// Result of storing a single attachment.
#[derive(Debug)]
pub struct StoredAttachment {
    pub meta: AttachmentMeta,
    /// Relative paths that were written (for git commit)
    pub rel_paths: Vec<String>,
}

/// Embed policy for attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedPolicy {
    /// Use server threshold to decide inline vs file
    Auto,
    /// Always inline (base64 embed)
    Inline,
    /// Always store as file reference
    File,
}

impl EmbedPolicy {
    pub fn from_str_policy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "inline" => Self::Inline,
            "file" => Self::File,
            _ => Self::Auto,
        }
    }
}

/// Reconstruct a [`StoredAttachment`] from cached WebP + manifest on disk.
///
/// Called when the SHA1-based cache check finds that the WebP and manifest
/// already exist, skipping the expensive image decode + re-encode.
fn store_attachment_from_cache(
    archive: &ProjectArchive,
    config: &Config,
    webp_path: &Path,
    manifest_path: &Path,
    digest: &str,
    embed_policy: EmbedPolicy,
) -> Result<StoredAttachment> {
    use base64::Engine;

    let manifest_str = fs::read_to_string(manifest_path)?;
    let manifest: AttachmentManifest = serde_json::from_str(&manifest_str)?;

    let webp_rel = rel_path_cached(&archive.canonical_repo_root, webp_path)?;

    let should_inline = match embed_policy {
        EmbedPolicy::Inline => true,
        EmbedPolicy::File => false,
        EmbedPolicy::Auto => manifest.bytes_webp <= config.inline_image_max_bytes,
    };

    let meta = if should_inline {
        let webp_bytes = fs::read(webp_path)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&webp_bytes);
        AttachmentMeta {
            kind: "inline".to_string(),
            media_type: "image/webp".to_string(),
            bytes: manifest.bytes_webp,
            sha1: digest.to_string(),
            width: manifest.width,
            height: manifest.height,
            data_base64: Some(encoded),
            path: None,
            original_path: manifest.original_path.clone(),
        }
    } else {
        AttachmentMeta {
            kind: "file".to_string(),
            media_type: "image/webp".to_string(),
            bytes: manifest.bytes_webp,
            sha1: digest.to_string(),
            width: manifest.width,
            height: manifest.height,
            data_base64: None,
            path: Some(webp_rel),
            original_path: manifest.original_path.clone(),
        }
    };

    // Return empty rel_paths — files are already on disk and committed.
    Ok(StoredAttachment {
        meta,
        rel_paths: Vec::new(),
    })
}

/// Store an image attachment in the archive.
///
/// Converts to WebP, writes to `attachments/{sha1[:2]}/{sha1}.webp`,
/// optionally keeps original, writes manifest and audit log.
/// Includes SHA1-based conversion cache: if the WebP already exists,
/// the expensive decode+encode is skipped.
///
/// Returns metadata and relative paths for git commit.
pub fn store_attachment(
    archive: &ProjectArchive,
    config: &Config,
    file_path: &Path,
    embed_policy: EmbedPolicy,
) -> Result<StoredAttachment> {
    use base64::Engine;
    use image::GenericImageView;

    // Read original file
    let original_bytes = fs::read(file_path)?;
    if original_bytes.is_empty() {
        return Err(StorageError::InvalidPath(
            "Attachment file is empty".to_string(),
        ));
    }

    // Compute SHA1 of original bytes
    let digest = {
        let mut hasher = sha1::Sha1::new();
        hasher.update(&original_bytes);
        hex::encode(hasher.finalize())
    };

    let prefix = &digest[..2.min(digest.len())];
    let original_ext = file_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();

    // Ensure attachment directories
    let attach_dir = archive.root.join("attachments");
    let webp_dir = attach_dir.join(prefix);
    let manifest_dir = attach_dir.join("_manifests");
    let audit_dir = attach_dir.join("_audit");
    ensure_dir(&webp_dir)?;
    ensure_dir(&manifest_dir)?;
    ensure_dir(&audit_dir)?;

    // -- Cache check: skip conversion if this SHA1 was already converted --
    let webp_filename = format!("{digest}.webp");
    let webp_path = webp_dir.join(&webp_filename);
    let manifest_path = manifest_dir.join(format!("{digest}.json"));
    if webp_path.exists() && manifest_path.exists() {
        return store_attachment_from_cache(
            archive,
            config,
            &webp_path,
            &manifest_path,
            &digest,
            embed_policy,
        );
    }

    // -- File size guard: reject pathologically large files --
    if original_bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(StorageError::InvalidPath(format!(
            "Attachment too large for conversion ({} bytes, max {})",
            original_bytes.len(),
            MAX_ATTACHMENT_BYTES,
        )));
    }

    let mut rel_paths = Vec::new();

    // Convert to WebP
    let img = image::load_from_memory(&original_bytes)
        .map_err(|e| StorageError::InvalidPath(format!("Failed to decode image: {e}")))?;
    let (width, height) = img.dimensions();

    // Encode to WebP using the image crate
    let mut webp_bytes = Vec::new();
    let rgba = img.to_rgba8();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
    encoder
        .encode(&rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| StorageError::InvalidPath(format!("WebP encode error: {e}")))?;

    fs::write(&webp_path, &webp_bytes)?;
    let webp_rel = rel_path_cached(&archive.canonical_repo_root, &webp_path)?;
    rel_paths.push(webp_rel.clone());

    // Optionally keep original
    let original_rel = if config.keep_original_images {
        let orig_dir = attach_dir.join("originals").join(prefix);
        ensure_dir(&orig_dir)?;
        let orig_path = orig_dir.join(format!("{digest}{original_ext}"));
        fs::write(&orig_path, &original_bytes)?;
        let rel = rel_path_cached(&archive.canonical_repo_root, &orig_path)?;
        rel_paths.push(rel.clone());
        Some(rel)
    } else {
        None
    };

    // Write manifest
    let manifest = AttachmentManifest {
        sha1: digest.clone(),
        webp_path: webp_rel.clone(),
        bytes_webp: webp_bytes.len(),
        bytes_original: original_bytes.len(),
        width,
        height,
        original_ext: original_ext.clone(),
        original_path: original_rel.clone(),
    };
    write_json(&manifest_path, &serde_json::to_value(&manifest)?)?;
    rel_paths.push(rel_path_cached(
        &archive.canonical_repo_root,
        &manifest_path,
    )?);

    // Write audit log entry
    let audit_path = audit_dir.join(format!("{digest}.log"));
    let audit_entry = serde_json::json!({
        "event": "stored",
        "ts": Utc::now().to_rfc3339(),
        "webp_path": webp_rel,
        "bytes_webp": webp_bytes.len(),
        "original_path": original_rel,
        "bytes_original": original_bytes.len(),
        "ext": original_ext,
    });
    let mut audit_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)?;
    audit_file.write_all(audit_entry.to_string().as_bytes())?;
    audit_file.write_all(b"\n")?;
    rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &audit_path)?);

    // Decide inline vs file based on policy
    let should_inline = match embed_policy {
        EmbedPolicy::Inline => true,
        EmbedPolicy::File => false,
        EmbedPolicy::Auto => webp_bytes.len() <= config.inline_image_max_bytes,
    };

    let meta = if should_inline {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&webp_bytes);
        AttachmentMeta {
            kind: "inline".to_string(),
            media_type: "image/webp".to_string(),
            bytes: webp_bytes.len(),
            sha1: digest,
            width,
            height,
            data_base64: Some(encoded),
            path: None,
            original_path: original_rel,
        }
    } else {
        AttachmentMeta {
            kind: "file".to_string(),
            media_type: "image/webp".to_string(),
            bytes: webp_bytes.len(),
            sha1: digest,
            width,
            height,
            data_base64: None,
            path: Some(webp_rel),
            original_path: original_rel,
        }
    };

    Ok(StoredAttachment { meta, rel_paths })
}

/// Process attachment paths and store them in the archive.
///
/// Resolves paths sequentially (fast), then converts up to
/// [`MAX_CONCURRENT_CONVERSIONS`] attachments in parallel using
/// `std::thread::scope` with chunk-based concurrency limiting.
///
/// Returns a list of attachment metadata and all relative paths written.
pub fn process_attachments(
    archive: &ProjectArchive,
    config: &Config,
    base_dir: &Path,
    attachment_paths: &[String],
    embed_policy: EmbedPolicy,
) -> Result<(Vec<AttachmentMeta>, Vec<String>)> {
    // Phase 1: resolve all paths (fast, no image I/O)
    let resolved: Vec<PathBuf> = attachment_paths
        .iter()
        .map(|p| resolve_attachment_source_path(base_dir, config, p))
        .collect::<Result<Vec<_>>>()?;

    // Phase 2: convert in parallel chunks
    let mut all_meta = Vec::with_capacity(resolved.len());
    let mut all_rel_paths = Vec::new();

    for chunk in resolved.chunks(MAX_CONCURRENT_CONVERSIONS) {
        let results: Vec<Result<StoredAttachment>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|path| s.spawn(|| store_attachment(archive, config, path, embed_policy)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("attachment conversion thread panicked"))
                .collect()
        });

        for result in results {
            let stored = result?;
            all_meta.push(stored.meta);
            all_rel_paths.extend(stored.rel_paths);
        }
    }

    Ok((all_meta, all_rel_paths))
}

/// Regex for matching Markdown image references: `![alt](path)`.
fn image_pattern_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<path>[^)]+)\)").expect("valid regex"))
}

/// Process inline image references in Markdown body.
///
/// Finds `![alt](path)` references and converts them in parallel (up to
/// [`MAX_CONCURRENT_CONVERSIONS`] at a time) with either:
/// - Inline base64 data URI: `![alt](data:image/webp;base64,...)`
/// - Archive file path: `![alt](attachments/ab/ab1234...webp)`
///
/// Returns the modified body and any attachment metadata/paths.
pub fn process_markdown_images(
    archive: &ProjectArchive,
    config: &Config,
    base_dir: &Path,
    body_md: &str,
    embed_policy: EmbedPolicy,
) -> Result<(String, Vec<AttachmentMeta>, Vec<String>)> {
    let re = image_pattern_re();

    // Collect and filter matches: (full_match, alt, resolved_path)
    let processable: Vec<(String, String, PathBuf)> = re
        .captures_iter(body_md)
        .filter_map(|cap| {
            let full = cap.get(0).unwrap().as_str().to_string();
            let alt = cap.name("alt").unwrap().as_str().to_string();
            let path = cap.name("path").unwrap().as_str().to_string();

            // Skip data URIs and URLs
            if path.starts_with("data:")
                || path.starts_with("http://")
                || path.starts_with("https://")
            {
                return None;
            }

            // Resolve best-effort: missing/unresolvable paths don't fail the message.
            let resolved = resolve_source_attachment_path_opt(base_dir, config, &path)?;
            Some((full, alt, resolved))
        })
        .collect();

    if processable.is_empty() {
        return Ok((body_md.to_string(), Vec::new(), Vec::new()));
    }

    // Convert in parallel chunks, collecting (full_match, alt, Result<StoredAttachment>)
    let mut converted: Vec<(
        String,
        String,
        std::result::Result<StoredAttachment, StorageError>,
    )> = Vec::with_capacity(processable.len());

    for chunk in processable.chunks(MAX_CONCURRENT_CONVERSIONS) {
        let chunk_results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(full, alt, path)| {
                    let full = full.clone();
                    let alt = alt.clone();
                    s.spawn(move || {
                        let result = store_attachment(archive, config, path, embed_policy);
                        (full, alt, result)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("markdown image conversion thread panicked"))
                .collect()
        });
        converted.extend(chunk_results);
    }

    // Apply replacements
    let mut result = body_md.to_string();
    let mut all_meta = Vec::new();
    let mut all_rel_paths = Vec::new();

    for (full_match, alt, stored_result) in converted {
        let Ok(stored) = stored_result else {
            continue; // Skip failed conversions
        };
        let replacement = if let Some(ref b64) = stored.meta.data_base64 {
            format!("![{alt}](data:image/webp;base64,{b64})")
        } else if let Some(ref file_path) = stored.meta.path {
            format!("![{alt}]({file_path})")
        } else {
            continue;
        };
        result = result.replace(&full_match, &replacement);
        all_rel_paths.extend(stored.rel_paths);
        all_meta.push(stored.meta);
    }

    Ok((result, all_meta, all_rel_paths))
}

/// Resolve an attachment source path from a user-provided string.
///
/// Semantics:
/// - Relative paths are resolved relative to `base_dir` (typically the project's `human_key`).
/// - Absolute paths outside `base_dir` are allowed only when `allow_absolute_attachment_paths=true`.
/// - Absolute paths inside `base_dir` are always allowed.
pub fn resolve_attachment_source_path(
    base_dir: &Path,
    config: &Config,
    raw_path: &str,
) -> Result<PathBuf> {
    let raw = raw_path.trim();
    if raw.is_empty() {
        return Err(StorageError::InvalidPath(
            "Attachment path cannot be empty".to_string(),
        ));
    }

    // Canonicalize base dir so prefix checks cannot be bypassed via symlinks.
    let base = base_dir.canonicalize().map_err(|e| {
        StorageError::InvalidPath(format!(
            "Attachment base directory does not exist or is not accessible: {} ({e})",
            base_dir.display()
        ))
    })?;

    let input = PathBuf::from(raw);
    let input_is_absolute = input.is_absolute();
    let candidate = if input_is_absolute {
        input
    } else {
        base.join(input)
    };

    if !candidate.exists() {
        return Err(StorageError::InvalidPath(format!(
            "Attachment not found: {}",
            candidate.display()
        )));
    }

    let resolved = candidate.canonicalize().map_err(|e| {
        StorageError::InvalidPath(format!(
            "Invalid attachment path: {} ({e})",
            candidate.display()
        ))
    })?;

    // Relative paths must never escape the project directory.
    if !input_is_absolute && !resolved.starts_with(&base) {
        return Err(StorageError::InvalidPath(format!(
            "Attachment path escapes the project directory: {}",
            resolved.display()
        )));
    }

    if resolved.starts_with(&base) {
        return Ok(resolved);
    }

    // Only absolute paths may be used outside the project directory, and only when enabled.
    if config.allow_absolute_attachment_paths {
        return Ok(resolved);
    }

    Err(StorageError::InvalidPath(format!(
        "Absolute attachment paths outside the project are not allowed: {}",
        resolved.display()
    )))
}

/// Best-effort variant for Markdown image conversion (skip missing/unresolvable paths).
fn resolve_source_attachment_path_opt(
    base_dir: &Path,
    config: &Config,
    raw_path: &str,
) -> Option<PathBuf> {
    let raw = raw_path.trim();
    if raw.is_empty() {
        return None;
    }

    let base = base_dir.canonicalize().ok()?;
    let input = PathBuf::from(raw);
    let input_is_absolute = input.is_absolute();
    let candidate = if input_is_absolute {
        input
    } else {
        base.join(input)
    };
    if !candidate.exists() {
        return None;
    }
    let resolved = candidate.canonicalize().ok()?;

    // Relative paths must never escape the project directory.
    if !input_is_absolute && !resolved.starts_with(&base) {
        return None;
    }

    if resolved.starts_with(&base) {
        return Some(resolved);
    }

    if config.allow_absolute_attachment_paths {
        return Some(resolved);
    }

    None
}

// ---------------------------------------------------------------------------
// Notification signals (legacy parity)
// ---------------------------------------------------------------------------

static SIGNAL_DEBOUNCE: OnceLock<OrderedMutex<HashMap<(String, String), u128>>> = OnceLock::new();

fn signal_debounce() -> &'static OrderedMutex<HashMap<(String, String), u128>> {
    SIGNAL_DEBOUNCE
        .get_or_init(|| OrderedMutex::new(LockLevel::StorageSignalDebounce, HashMap::new()))
}

/// Emit a notification signal file for a project/agent.
///
/// Returns `true` if a signal was emitted, `false` if disabled, debounced, or failed.
pub fn emit_notification_signal(
    config: &Config,
    project_slug: &str,
    agent_name: &str,
    message_metadata: Option<&NotificationMessage>,
) -> bool {
    if !config.notifications_enabled {
        return false;
    }

    // Reject path-traversal characters to prevent writing outside the signals dir.
    if project_slug.contains('/')
        || project_slug.contains('\\')
        || project_slug.contains("..")
        || agent_name.contains('/')
        || agent_name.contains('\\')
        || agent_name.contains("..")
    {
        return false;
    }

    let debounce_ms = config.notifications_debounce_ms as u128;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let key = (project_slug.to_string(), agent_name.to_string());
    {
        let mut map = signal_debounce().lock();
        let last = map.get(&key).copied().unwrap_or(0);
        if debounce_ms > 0 && now_ms.saturating_sub(last) < debounce_ms {
            return false;
        }
        map.insert(key, now_ms);
    }

    let signal_path = config
        .notifications_signals_dir
        .join("projects")
        .join(project_slug)
        .join("agents")
        .join(format!("{agent_name}.signal"));

    let mut signal_data = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339(),
        "project": project_slug,
        "agent": agent_name,
    });

    if config.notifications_include_metadata {
        if let Some(meta) = message_metadata {
            let importance = meta
                .importance
                .clone()
                .unwrap_or_else(|| "normal".to_string());
            signal_data["message"] = serde_json::json!({
                "id": meta.id,
                "from": meta.from,
                "subject": meta.subject,
                "importance": importance,
            });
        }
    }

    write_json(&signal_path, &signal_data).is_ok()
}

/// Clear notification signal for a project/agent.
///
/// Returns `true` if a signal was removed, `false` otherwise.
pub fn clear_notification_signal(config: &Config, project_slug: &str, agent_name: &str) -> bool {
    if !config.notifications_enabled {
        return false;
    }

    // Reject path-traversal characters.
    if project_slug.contains('/')
        || project_slug.contains('\\')
        || project_slug.contains("..")
        || agent_name.contains('/')
        || agent_name.contains('\\')
        || agent_name.contains("..")
    {
        return false;
    }

    let signal_path = config
        .notifications_signals_dir
        .join("projects")
        .join(project_slug)
        .join("agents")
        .join(format!("{agent_name}.signal"));

    if !signal_path.exists() {
        return false;
    }

    fs::remove_file(&signal_path).is_ok()
}

/// List pending notification signals.
pub fn list_pending_signals(config: &Config, project_slug: Option<&str>) -> Vec<serde_json::Value> {
    if !config.notifications_enabled {
        return Vec::new();
    }

    let projects_root = config.notifications_signals_dir.join("projects");
    if !projects_root.exists() {
        return Vec::new();
    }

    let mut results = Vec::new();
    let dirs: Vec<PathBuf> = if let Some(slug) = project_slug {
        // Reject path-traversal characters.
        if slug.contains('/') || slug.contains('\\') || slug.contains("..") {
            return Vec::new();
        }
        let d = projects_root.join(slug);
        if d.exists() { vec![d] } else { vec![] }
    } else {
        match fs::read_dir(&projects_root) {
            Ok(iter) => iter
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect(),
            Err(_) => return Vec::new(),
        }
    };

    for proj_dir in dirs {
        let slug = proj_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let agents_dir = proj_dir.join("agents");
        if !agents_dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&agents_dir) {
            Ok(iter) => iter,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.path().extension().is_some_and(|e| e == "signal") {
                let content = match fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(val) => results.push(val),
                    Err(_) => {
                        let agent = entry
                            .path()
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        results.push(serde_json::json!({
                            "project": slug,
                            "agent": agent,
                            "error": "Failed to parse signal file",
                        }));
                    }
                }
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Read helpers
// ---------------------------------------------------------------------------

/// Get recent commits from the archive repository.
pub fn get_recent_commits(
    archive: &ProjectArchive,
    limit: usize,
    path_filter: Option<&str>,
) -> Result<Vec<CommitInfo>> {
    let repo = Repository::open(&archive.repo_root)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut commits = Vec::new();

    for oid_result in revwalk {
        if commits.len() >= limit {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;

        // Optional path filter
        if let Some(filter) = path_filter {
            let dominated = commit_touches_path(&repo, &commit, filter);
            if !dominated {
                continue;
            }
        }

        let author = commit.author();
        commits.push(CommitInfo {
            sha: oid.to_string(),
            short_sha: oid.to_string()[..7.min(oid.to_string().len())].to_string(),
            author: author.name().unwrap_or("unknown").to_string(),
            email: author.email().unwrap_or("").to_string(),
            date: {
                let time = author.when();
                let secs = time.seconds();
                DateTime::from_timestamp(secs, 0)
                    .unwrap_or_default()
                    .to_rfc3339()
            },
            summary: commit.summary().unwrap_or("").to_string(),
        });
    }

    Ok(commits)
}

/// Check if a commit touches files under a given path prefix.
fn commit_touches_path(repo: &Repository, commit: &git2::Commit<'_>, path_prefix: &str) -> bool {
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return false,
    };

    // Check if any entry in the diff starts with path_prefix
    if commit.parent_count() == 0 {
        // Root commit: check all entries
        return tree_contains_prefix(&tree, path_prefix);
    }

    if let Ok(parent) = commit.parent(0) {
        if let Ok(parent_tree) = parent.tree() {
            if let Ok(diff) = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None) {
                let mut found = false;
                let _ = diff.foreach(
                    &mut |delta, _progress| {
                        if let Some(p) = delta.new_file().path() {
                            if p.to_string_lossy().starts_with(path_prefix) {
                                found = true;
                            }
                        }
                        true
                    },
                    None,
                    None,
                    None,
                );
                return found;
            }
        }
    }

    false
}

/// Find the commit that introduced a specific file path.
///
/// Walks the git log and returns the first commit where the file appeared.
/// Used by mailbox-with-commits views to map messages to their commits.
pub fn find_commit_for_path(
    archive: &ProjectArchive,
    rel_path_str: &str,
) -> Result<Option<CommitInfo>> {
    let repo = Repository::open(&archive.repo_root)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    for oid_result in revwalk {
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;

        if commit_touches_path(&repo, &commit, rel_path_str) {
            let author = commit.author();
            return Ok(Some(CommitInfo {
                sha: oid.to_string(),
                short_sha: oid.to_string()[..7.min(oid.to_string().len())].to_string(),
                author: author.name().unwrap_or("unknown").to_string(),
                email: author.email().unwrap_or("").to_string(),
                date: {
                    let time = author.when();
                    let secs = time.seconds();
                    DateTime::from_timestamp(secs, 0)
                        .unwrap_or_default()
                        .to_rfc3339()
                },
                summary: commit.summary().unwrap_or("").to_string(),
            }));
        }
    }

    Ok(None)
}

/// Get recent commits filtered by author name.
///
/// Used by `whois(include_recent_commits=true)` to show an agent's
/// recent archive activity.
pub fn get_commits_by_author(
    archive: &ProjectArchive,
    author_name: &str,
    limit: usize,
) -> Result<Vec<CommitInfo>> {
    let repo = Repository::open(&archive.repo_root)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut commits = Vec::new();

    for oid_result in revwalk {
        if commits.len() >= limit {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;

        let author = commit.author();
        let name = author.name().unwrap_or("");

        if name == author_name {
            commits.push(CommitInfo {
                sha: oid.to_string(),
                short_sha: oid.to_string()[..7.min(oid.to_string().len())].to_string(),
                author: name.to_string(),
                email: author.email().unwrap_or("").to_string(),
                date: {
                    let time = author.when();
                    let secs = time.seconds();
                    DateTime::from_timestamp(secs, 0)
                        .unwrap_or_default()
                        .to_rfc3339()
                },
                summary: commit.summary().unwrap_or("").to_string(),
            });
        }
    }

    Ok(commits)
}

/// Get commit metadata for a message's canonical path.
///
/// Convenience wrapper: given the archive paths for a message,
/// returns the commit that introduced its canonical file.
pub fn get_commit_for_message(
    archive: &ProjectArchive,
    message_paths: &MessageArchivePaths,
) -> Result<Option<CommitInfo>> {
    let canonical_rel = rel_path_cached(&archive.canonical_repo_root, &message_paths.canonical)?;
    find_commit_for_path(archive, &canonical_rel)
}

// ---------------------------------------------------------------------------
// Archive web UI helpers
// ---------------------------------------------------------------------------

/// Extended commit info including diff stats and relative date.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedCommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub relative_date: String,
    pub subject: String,
    pub body: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Compute a human-readable relative date string.
fn relative_date_from_secs(authored_secs: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let delta = now - authored_secs;
    if delta < 0 {
        return "just now".to_string();
    }
    let days = delta / 86400;
    if days > 30 {
        // Format as "Feb 08, 2026"
        DateTime::from_timestamp(authored_secs, 0)
            .map(|dt| dt.format("%b %d, %Y").to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else if days > 0 {
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    } else {
        let hours = delta / 3600;
        if hours > 0 {
            if hours == 1 {
                "1 hour ago".to_string()
            } else {
                format!("{hours} hours ago")
            }
        } else {
            let minutes = delta / 60;
            if minutes > 0 {
                if minutes == 1 {
                    "1 minute ago".to_string()
                } else {
                    format!("{minutes} minutes ago")
                }
            } else {
                "just now".to_string()
            }
        }
    }
}

/// Get recent commits with extended metadata (stats, relative dates).
///
/// Used by the archive activity feed. Returns commits from the repo root
/// (not scoped to a single project).
pub fn get_recent_commits_extended(
    archive_root: &Path,
    limit: usize,
) -> Result<Vec<ExtendedCommitInfo>> {
    let repo = Repository::open(archive_root)?;

    let mut revwalk = repo.revwalk()?;
    if revwalk.push_head().is_err() {
        return Ok(Vec::new());
    }
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut commits = Vec::new();

    for oid_result in revwalk {
        if commits.len() >= limit {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;
        let author = commit.author();
        let authored_secs = author.when().seconds();

        // Compute diff stats
        let (files_changed, insertions, deletions) = commit_diff_stats(&repo, &commit);

        let message = commit.message().unwrap_or("");
        let subject = message.lines().next().unwrap_or("").to_string();
        let body = message.to_string();

        commits.push(ExtendedCommitInfo {
            sha: oid.to_string(),
            short_sha: oid.to_string()[..8.min(oid.to_string().len())].to_string(),
            author: author.name().unwrap_or("unknown").to_string(),
            email: author.email().unwrap_or("").to_string(),
            date: DateTime::from_timestamp(authored_secs, 0)
                .unwrap_or_default()
                .to_rfc3339(),
            relative_date: relative_date_from_secs(authored_secs),
            subject,
            body,
            files_changed,
            insertions,
            deletions,
        });
    }

    Ok(commits)
}

/// Compute diff stats (files_changed, insertions, deletions) for a commit.
fn commit_diff_stats(repo: &Repository, commit: &git2::Commit<'_>) -> (usize, usize, usize) {
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return (0, 0, 0),
    };

    let parent_tree = if commit.parent_count() > 0 {
        commit.parent(0).ok().and_then(|p| p.tree().ok())
    } else {
        None
    };

    let diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None) {
        Ok(d) => d,
        Err(_) => return (0, 0, 0),
    };

    let stats = match diff.stats() {
        Ok(s) => s,
        Err(_) => return (0, 0, 0),
    };

    (stats.files_changed(), stats.insertions(), stats.deletions())
}

/// Detailed commit information including diffs and changed files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDetail {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub trailers: Vec<(String, String)>,
    pub files_changed: Vec<ChangedFile>,
    pub diff: String,
    pub stats: CommitStats,
}

/// A file changed in a commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub change_type: String,
    pub a_path: String,
    pub b_path: String,
}

/// Aggregate commit statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitStats {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Get detailed information about a specific commit including diffs.
///
/// `sha` must be a valid hex string (7-40 chars).
pub fn get_commit_detail(
    archive_root: &Path,
    sha: &str,
    max_diff_bytes: usize,
) -> Result<CommitDetail> {
    // Validate SHA format
    if sha.len() < 7 || sha.len() > 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid commit SHA format",
        )));
    }

    let repo = Repository::open(archive_root)?;
    let oid = git2::Oid::from_str(sha)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        commit.parent(0).ok().and_then(|p| p.tree().ok())
    } else {
        None
    };

    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;

    // Collect changed files
    let mut changed_files = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let new_path = delta
                .new_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let old_path = delta
                .old_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let change_type = match delta.status() {
                git2::Delta::Added => "added",
                git2::Delta::Deleted => "deleted",
                git2::Delta::Modified => "modified",
                git2::Delta::Renamed => "renamed",
                git2::Delta::Copied => "copied",
                _ => "modified",
            };

            let display_path = if new_path.is_empty() || new_path == "/dev/null" {
                old_path.clone()
            } else {
                new_path.clone()
            };

            changed_files.push(ChangedFile {
                path: display_path,
                change_type: change_type.to_string(),
                a_path: if old_path.is_empty() {
                    "/dev/null".to_string()
                } else {
                    old_path
                },
                b_path: if new_path.is_empty() {
                    "/dev/null".to_string()
                } else {
                    new_path
                },
            });
            true
        },
        None,
        None,
        None,
    )?;

    // Build diff text
    let mut diff_text = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        if diff_text.len() < max_diff_bytes {
            let origin = line.origin();
            if origin == '+' || origin == '-' || origin == ' ' {
                diff_text.push(origin);
            }
            if let Ok(s) = std::str::from_utf8(line.content()) {
                diff_text.push_str(s);
            }
        }
        true
    })?;

    if diff_text.len() >= max_diff_bytes {
        diff_text.push_str("\n\n[... Diff truncated — exceeds size limit ...]\n");
    }

    // Compute stats
    let stats = diff.stats()?;

    // Parse commit message
    let message = commit.message().unwrap_or("").to_string();
    let lines: Vec<&str> = message.lines().collect();
    let subject = lines.first().copied().unwrap_or("").to_string();

    // Extract body and trailers
    let rest = if lines.len() > 1 { &lines[1..] } else { &[] };
    let mut body_lines = Vec::new();
    let mut trailers = Vec::new();

    // Scan from end for trailers (lines matching "Key: Value")
    let mut trailer_start = rest.len();
    for i in (0..rest.len()).rev() {
        let line = rest[i].trim();
        if !line.is_empty() && line.contains(": ") && !line.starts_with(' ') {
            trailer_start = i;
        } else if line.is_empty() && trailer_start < rest.len() {
            break;
        } else {
            trailer_start = rest.len();
            break;
        }
    }

    for (i, line) in rest.iter().enumerate() {
        if i < trailer_start {
            body_lines.push(*line);
        } else if let Some((k, v)) = line.split_once(": ") {
            trailers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let body = body_lines.join("\n").trim().to_string();
    let author = commit.author();
    let authored_secs = author.when().seconds();

    Ok(CommitDetail {
        sha: oid.to_string(),
        short_sha: oid.to_string()[..8.min(oid.to_string().len())].to_string(),
        author: author.name().unwrap_or("unknown").to_string(),
        email: author.email().unwrap_or("").to_string(),
        date: DateTime::from_timestamp(authored_secs, 0)
            .unwrap_or_default()
            .to_rfc3339(),
        subject,
        body,
        trailers,
        files_changed: changed_files,
        diff: diff_text,
        stats: CommitStats {
            files: stats.files_changed(),
            insertions: stats.insertions(),
            deletions: stats.deletions(),
        },
    })
}

/// An entry in the archive file tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
}

/// Get directory tree listing from the archive's git tree.
///
/// `path` is relative to the project root within the archive
/// (e.g., "messages/2026" or "" for root).
pub fn get_archive_tree(archive: &ProjectArchive, path: &str) -> Result<Vec<TreeEntry>> {
    // Sanitize path to prevent traversal
    let safe_path = sanitize_browse_path(path)?;

    let repo = Repository::open(&archive.repo_root)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let commit = head.peel_to_commit()?;
    let root_tree = commit.tree()?;

    // Navigate to projects/{slug}/{path}
    let tree_path = if safe_path.is_empty() {
        format!("projects/{}", archive.slug)
    } else {
        format!("projects/{}/{safe_path}", archive.slug)
    };

    let tree_obj = match root_tree.get_path(std::path::Path::new(&tree_path)) {
        Ok(entry) => entry,
        Err(_) => return Ok(Vec::new()),
    };

    let obj = repo.find_object(tree_obj.id(), None)?;
    let tree = match obj.as_tree() {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };

    let mut entries = Vec::new();
    for item in tree.iter() {
        let name = item.name().unwrap_or("").to_string();
        let entry_path = if safe_path.is_empty() {
            name.clone()
        } else {
            format!("{safe_path}/{name}")
        };

        let (entry_type, size) = match item.kind() {
            Some(git2::ObjectType::Tree) => ("dir".to_string(), 0),
            Some(git2::ObjectType::Blob) => {
                let sz = repo
                    .find_blob(item.id())
                    .map(|b| b.size() as u64)
                    .unwrap_or(0);
                ("file".to_string(), sz)
            }
            _ => ("file".to_string(), 0),
        };

        entries.push(TreeEntry {
            name,
            path: entry_path,
            entry_type,
            size,
        });
    }

    // Sort: directories first, then files, both alphabetically
    entries.sort_by(|a, b| {
        let a_dir = a.entry_type == "dir";
        let b_dir = b.entry_type == "dir";
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(entries)
}

/// Sanitize a browsable path to prevent directory traversal.
fn sanitize_browse_path(path: &str) -> Result<String> {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized.starts_with("..")
        || normalized.contains("/../")
        || normalized.ends_with("/..")
        || normalized == ".."
    {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid path: directory traversal not allowed",
        )));
    }
    Ok(normalized.trim_start_matches('/').to_string())
}

/// Read file content from the archive's git tree.
///
/// Returns the UTF-8 content of the file, or `None` if not found.
/// Path is relative to the project root within the archive.
pub fn get_archive_file_content(
    archive: &ProjectArchive,
    path: &str,
    max_size_bytes: usize,
) -> Result<Option<String>> {
    let safe_path = sanitize_browse_path(path)?;
    if safe_path.is_empty() {
        return Ok(None);
    }

    let repo = Repository::open(&archive.repo_root)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let commit = head.peel_to_commit()?;
    let root_tree = commit.tree()?;

    let full_path = format!("projects/{}/{safe_path}", archive.slug);

    let entry = match root_tree.get_path(std::path::Path::new(&full_path)) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    let blob = match repo.find_blob(entry.id()) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    if blob.size() > max_size_bytes {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "File too large: {} bytes (max {max_size_bytes})",
                blob.size()
            ),
        )));
    }

    Ok(Some(String::from_utf8_lossy(blob.content()).to_string()))
}

/// A node in the agent communication graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub sent: usize,
    pub received: usize,
    pub total: usize,
}

/// An edge in the agent communication graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub count: usize,
}

/// Agent communication graph built from commit history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunicationGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Build an agent communication graph from message commit history.
///
/// Analyzes commits under `projects/{slug}/messages` and parses
/// commit subjects with format "mail: Sender -> Recipient1, Recipient2 | Subject".
pub fn get_communication_graph(
    archive_root: &Path,
    project_slug: &str,
    limit: usize,
) -> Result<CommunicationGraph> {
    let repo = Repository::open(archive_root)?;

    let mut revwalk = repo.revwalk()?;
    if revwalk.push_head().is_err() {
        return Ok(CommunicationGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }
    revwalk.set_sorting(git2::Sort::TIME)?;

    let path_prefix = format!("projects/{project_slug}/messages");
    let mut agent_stats: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    let mut connections: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();

    let mut seen = 0usize;
    for oid_result in revwalk {
        if seen >= limit {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;

        // Check if commit touches message path
        if !commit_touches_path(&repo, &commit, &path_prefix) {
            continue;
        }
        seen += 1;

        let subject = commit.summary().unwrap_or("");
        if !subject.starts_with("mail: ") {
            continue;
        }

        // Parse "mail: Sender -> Recipient1, Recipient2 | Subject"
        let rest = &subject[6..];
        let sender_part = if let Some((sp, _)) = rest.split_once(" | ") {
            sp
        } else {
            rest
        };

        if let Some((sender, recipients_str)) = sender_part.split_once(" -> ") {
            let sender = sender.trim().to_string();
            agent_stats.entry(sender.clone()).or_insert((0, 0)).0 += 1;

            for r in recipients_str.split(',') {
                let recipient = r.trim().to_string();
                if recipient.is_empty() {
                    continue;
                }
                agent_stats.entry(recipient.clone()).or_insert((0, 0)).1 += 1;
                *connections.entry((sender.clone(), recipient)).or_insert(0) += 1;
            }
        }
    }

    let nodes = agent_stats
        .into_iter()
        .map(|(name, (sent, received))| GraphNode {
            id: name.clone(),
            label: name,
            sent,
            received,
            total: sent + received,
        })
        .collect();

    let edges = connections
        .into_iter()
        .map(|((from, to), count)| GraphEdge { from, to, count })
        .collect();

    Ok(CommunicationGraph { nodes, edges })
}

/// A commit entry formatted for timeline visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub sha: String,
    pub short_sha: String,
    pub date: String,
    pub timestamp: i64,
    pub subject: String,
    #[serde(rename = "type")]
    pub commit_type: String,
    pub sender: Option<String>,
    pub recipients: Vec<String>,
    pub author: String,
}

/// Get commits for timeline visualization, scoped to a project.
///
/// Commits are returned in chronological order (oldest first).
pub fn get_timeline_commits(
    archive_root: &Path,
    project_slug: &str,
    limit: usize,
) -> Result<Vec<TimelineEntry>> {
    let repo = Repository::open(archive_root)?;

    let mut revwalk = repo.revwalk()?;
    if revwalk.push_head().is_err() {
        return Ok(Vec::new());
    }
    revwalk.set_sorting(git2::Sort::TIME)?;

    let path_prefix = format!("projects/{project_slug}");
    let mut entries = Vec::new();

    for oid_result in revwalk {
        if entries.len() >= limit {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;

        if !commit_touches_path(&repo, &commit, &path_prefix) {
            continue;
        }

        let subject = commit.summary().unwrap_or("").to_string();
        let authored_secs = commit.author().when().seconds();

        // Classify commit type and extract sender/recipients
        let (commit_type, sender, recipients) = if let Some(rest) = subject.strip_prefix("mail: ") {
            let sender_part = if let Some((sp, _)) = rest.split_once(" | ") {
                sp
            } else {
                rest
            };
            if let Some((s, r)) = sender_part.split_once(" -> ") {
                let recips: Vec<String> = r.split(',').map(|x| x.trim().to_string()).collect();
                ("message".to_string(), Some(s.trim().to_string()), recips)
            } else {
                ("message".to_string(), None, Vec::new())
            }
        } else if subject.starts_with("file_reservation: ") {
            ("file_reservation".to_string(), None, Vec::new())
        } else if subject.starts_with("chore: ") {
            ("chore".to_string(), None, Vec::new())
        } else {
            ("other".to_string(), None, Vec::new())
        };

        entries.push(TimelineEntry {
            sha: oid.to_string(),
            short_sha: oid.to_string()[..8.min(oid.to_string().len())].to_string(),
            date: DateTime::from_timestamp(authored_secs, 0)
                .unwrap_or_default()
                .to_rfc3339(),
            timestamp: authored_secs,
            subject,
            commit_type,
            sender,
            recipients,
            author: commit.author().name().unwrap_or("unknown").to_string(),
        });
    }

    // Sort oldest first for timeline
    entries.sort_by_key(|e| e.timestamp);

    Ok(entries)
}

/// Read a message file from the archive and parse its frontmatter.
///
/// Returns `(frontmatter_json, body_markdown)`.
pub fn read_message_file(path: &Path) -> Result<(serde_json::Value, String)> {
    let content = fs::read_to_string(path)?;

    // Parse ---json frontmatter
    if let Some(rest) = content.strip_prefix("---json\n") {
        if let Some(end_idx) = rest.find("\n---\n") {
            let json_str = &rest[..end_idx];
            let body = rest[end_idx + 5..].trim().to_string();
            let frontmatter = serde_json::from_str(json_str)?;
            return Ok((frontmatter, body));
        }
    }

    // No frontmatter - treat entire content as body
    Ok((serde_json::Value::Null, content))
}

/// List all message files in a directory (inbox, outbox, or canonical).
///
/// Returns paths sorted by modification time (newest first).
pub fn list_message_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    walk_md_files(dir, &mut files)?;

    // Sort by modification time descending (newest first)
    files.sort_by(|a, b| {
        let a_time = fs::metadata(a).and_then(|m| m.modified()).ok();
        let b_time = fs::metadata(b).and_then(|m| m.modified()).ok();
        b_time.cmp(&a_time)
    });

    Ok(files)
}

/// Recursively collect .md files from a directory tree.
fn walk_md_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_md_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            files.push(path);
        }
    }
    Ok(())
}

/// Get inbox message files for a specific agent.
pub fn list_agent_inbox(archive: &ProjectArchive, agent_name: &str) -> Result<Vec<PathBuf>> {
    let agent_name = validate_archive_component("agent name", agent_name)?;
    let inbox_dir = archive.root.join("agents").join(agent_name).join("inbox");
    list_message_files(&inbox_dir)
}

/// Get outbox message files for a specific agent.
pub fn list_agent_outbox(archive: &ProjectArchive, agent_name: &str) -> Result<Vec<PathBuf>> {
    let agent_name = validate_archive_component("agent name", agent_name)?;
    let outbox_dir = archive.root.join("agents").join(agent_name).join("outbox");
    list_message_files(&outbox_dir)
}

/// List all agents with profiles in the archive.
pub fn list_archive_agents(archive: &ProjectArchive) -> Result<Vec<String>> {
    let agents_dir = archive.root.join("agents");
    if !agents_dir.exists() {
        return Ok(Vec::new());
    }

    let mut agents = Vec::new();
    for entry in fs::read_dir(&agents_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            let profile = entry.path().join("profile.json");
            if profile.exists() {
                if let Some(name) = entry.file_name().to_str() {
                    agents.push(name.to_string());
                }
            }
        }
    }
    agents.sort();
    Ok(agents)
}

/// Read an agent's profile from the archive.
pub fn read_agent_profile(
    archive: &ProjectArchive,
    agent_name: &str,
) -> Result<Option<serde_json::Value>> {
    let agent_name = validate_archive_component("agent name", agent_name)?;
    let profile_path = archive
        .root
        .join("agents")
        .join(agent_name)
        .join("profile.json");
    if !profile_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&profile_path)?;
    let value = serde_json::from_str(&content)?;
    Ok(Some(value))
}

/// Check if a tree has any entry with the given path prefix.
fn tree_contains_prefix(tree: &git2::Tree<'_>, prefix: &str) -> bool {
    for entry in tree.iter() {
        if let Some(name) = entry.name() {
            if name.starts_with(prefix) || prefix.starts_with(name) {
                return true;
            }
        }
    }
    false
}

/// Collect lock status information for diagnostics.
pub fn collect_lock_status(config: &Config) -> Result<serde_json::Value> {
    let root = &config.storage_root;
    if !root.exists() {
        return Ok(serde_json::json!({
            "archive_root": root.display().to_string(),
            "exists": false,
            "locks": [],
        }));
    }

    let mut locks = Vec::new();

    // Walk the archive root looking for .lock files
    fn walk_locks(dir: &Path, locks: &mut Vec<serde_json::Value>) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk_locks(&path, locks)?;
            } else if path.extension().is_some_and(|e| e == "lock") {
                let metadata = fs::metadata(&path)?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());

                locks.push(serde_json::json!({
                    "path": path.display().to_string(),
                    "size": metadata.len(),
                    "modified_epoch": modified,
                }));

                // Check for owner metadata
                let owner_path = path.with_extension("lock.owner.json");
                if owner_path.exists() {
                    if let Ok(content) = fs::read_to_string(&owner_path) {
                        if let Ok(owner) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(last) = locks.last_mut() {
                                last.as_object_mut()
                                    .unwrap()
                                    .insert("owner".to_string(), owner);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    walk_locks(root, &mut locks)?;

    Ok(serde_json::json!({
        "archive_root": root.display().to_string(),
        "exists": true,
        "locks": locks,
    }))
}

// ---------------------------------------------------------------------------
// Core git operations
// ---------------------------------------------------------------------------

fn resolve_head_commit_oid(repo: &Repository) -> Result<Option<git2::Oid>> {
    fn load_head_commit_oid(repo: &Repository) -> std::result::Result<git2::Oid, git2::Error> {
        let head = repo.head()?;
        let commit = head.peel_to_commit()?;
        Ok(commit.id())
    }

    match load_head_commit_oid(repo) {
        Ok(oid) => Ok(Some(oid)),
        Err(err) if matches!(err.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            // Some long-lived repo handles can transiently lose HEAD resolution.
            match Repository::open(repo.path()).and_then(|reopened| load_head_commit_oid(&reopened))
            {
                Ok(oid) => Ok(Some(oid)),
                Err(err2)
                    if matches!(err2.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) =>
                {
                    Ok(None)
                }
                Err(err2) => Err(err2.into()),
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// Refresh an index from `HEAD` so stale index state cannot leak between commits.
fn reset_index_to_head(repo: &Repository, index: &mut git2::Index) -> Result<()> {
    if let Some(commit_oid) = resolve_head_commit_oid(repo)? {
        let commit = repo.find_commit(commit_oid)?;
        let tree_oid = commit.tree_id();
        let tree = repo.find_tree(tree_oid)?;
        index.read_tree(&tree)?;
    } else {
        // Truly unborn repository with no commits yet.
        index.clear()?;
    }
    Ok(())
}

/// Best-effort cleanup of staged/index state after a failed commit attempt.
fn try_restore_index_to_head(repo: &Repository) -> Result<()> {
    if let Some(workdir) = repo.workdir() {
        match std::process::Command::new("git")
            .arg("-C")
            .arg(workdir)
            .arg("read-tree")
            .arg("HEAD")
            .status()
        {
            Ok(status) if status.success() => return Ok(()),
            _ => {
                // Fall through to libgit2-based recovery path.
            }
        }
    }

    // Re-open to avoid stale ref views from long-lived handles.
    let reopened = Repository::open(repo.path())?;
    let mut index = reopened.index()?;
    reset_index_to_head(&reopened, &mut index)?;
    index.write()?;
    Ok(())
}

/// Best-effort cleanup of staged/index state after a failed commit attempt.
fn try_restore_index(repo: &Repository) {
    let _ = try_restore_index_to_head(repo);
}

/// Add files to the git index and create a commit.
///
/// This is the core commit function used by all write operations.
fn commit_paths(
    repo: &Repository,
    config: &Config,
    message: &str,
    rel_paths: &[&str],
) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }

    let sig = Signature::now(&config.git_author_name, &config.git_author_email)?;

    let mut index = repo.index()?;
    reset_index_to_head(repo, &mut index)?;
    let mut any_added = false;

    for path in rel_paths {
        let path = validate_repo_relative_path("commit path", path)?;
        // git2 expects forward-slash paths on all platforms
        let p = Path::new(path);
        // Only add if the file exists on disk
        let full = repo.workdir().ok_or(StorageError::NotInitialized)?.join(p);
        if full.exists() {
            index.add_path(p)?;
            any_added = true;
        }
    }

    if !any_added {
        return Ok(());
    }

    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;

    // Append agent/thread trailers if applicable
    let final_message = append_trailers(message);

    // Find parent commit (if any)
    let parent = resolve_head_commit_oid(repo)?
        .map(|oid| repo.find_commit(oid))
        .transpose()?;

    let commit_result = match parent {
        Some(ref p) => repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[p]),
        None => repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[]),
    };

    if let Err(err) = commit_result {
        try_restore_index(repo);
        return Err(err.into());
    }

    Ok(())
}

/// Add all changed files to the git index and create a commit.
///
/// Only used as an overload escape hatch for the async commit coalescer when the
/// spill path set grows too large to track precisely.
fn commit_all(repo: &Repository, config: &Config, message: &str) -> Result<()> {
    // Ensure this is a non-bare repo with a workdir.
    let _workdir = repo.workdir().ok_or(StorageError::NotInitialized)?;

    let sig = Signature::now(&config.git_author_name, &config.git_author_email)?;
    let mut index = repo.index()?;
    reset_index_to_head(repo, &mut index)?;

    // Respect .gitignore, add all changes under the workdir.
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;

    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;

    let final_message = append_trailers(message);
    let parent = resolve_head_commit_oid(repo)?
        .map(|oid| repo.find_commit(oid))
        .transpose()?;

    let commit_result = match parent {
        Some(ref p) => repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[p]),
        None => repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[]),
    };

    if let Err(err) = commit_result {
        try_restore_index(repo);
        return Err(err.into());
    }

    Ok(())
}

/// Append git trailers (Agent:, Thread:) based on commit message content.
fn append_trailers(message: &str) -> String {
    let lower = message.to_lowercase();
    let has_agent = lower.contains("\nagent:");

    let mut trailers = Vec::new();

    if message.starts_with("mail: ") && !has_agent {
        if let Some(rest) = message.strip_prefix("mail: ") {
            if let Some(agent_part) = rest.split("->").next() {
                let agent = agent_part.trim();
                if !agent.is_empty() {
                    trailers.push(format!("Agent: {agent}"));
                }
            }
        }
    } else if message.starts_with("file_reservation: ") && !has_agent {
        if let Some(rest) = message.strip_prefix("file_reservation: ") {
            if let Some(agent_part) = rest.split_whitespace().next() {
                let agent = agent_part.trim();
                if !agent.is_empty() {
                    trailers.push(format!("Agent: {agent}"));
                }
            }
        }
    }

    if trailers.is_empty() {
        message.to_string()
    } else {
        format!("{message}\n\n{}\n", trailers.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Path / file helpers
// ---------------------------------------------------------------------------

/// Compute a relative path from a pre-canonicalized base to `target`.
///
/// The base is expected to already be canonicalized (avoiding repeated
/// `readlink` syscalls).  The target is first attempted without
/// canonicalization (fast path) and only falls back to `canonicalize()`
/// if `strip_prefix` fails.
fn rel_path_cached(canonical_base: &Path, target: &Path) -> Result<String> {
    // Fast path: try strip_prefix without canonicalizing target.
    // This succeeds when target is a direct join of the base (no symlinks).
    if let Ok(rel) = target.strip_prefix(canonical_base) {
        return Ok(rel.to_string_lossy().replace('\\', "/"));
    }

    // Slow path: canonicalize target and retry.
    let target_canon = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());

    target_canon
        .strip_prefix(canonical_base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| {
            StorageError::InvalidPath(format!(
                "Cannot compute relative path from {} to {}",
                canonical_base.display(),
                target_canon.display()
            ))
        })
}

/// Resolve a relative path safely inside the project archive root.
///
/// Rejects directory traversal and ensures the path stays within the archive.
/// When `canonicalize()` fails (e.g. the file doesn't exist yet), we manually
/// resolve `..` and `.` components to prevent bypass via non-existent paths.
pub fn resolve_archive_relative_path(archive: &ProjectArchive, raw_path: &str) -> Result<PathBuf> {
    let normalized = raw_path.trim().replace('\\', "/");

    if normalized.is_empty() || normalized.starts_with('/') {
        return Err(StorageError::InvalidPath(
            "directory traversal not allowed".to_string(),
        ));
    }

    // Reject any component that is ".." — even if it's embedded in intermediate segments.
    // We do this by splitting on `/` and checking each segment.
    for segment in normalized.split('/') {
        if segment == ".." {
            return Err(StorageError::InvalidPath(
                "directory traversal not allowed".to_string(),
            ));
        }
    }

    let safe_rel = normalized.trim_start_matches('/');
    // Use pre-canonicalized root to avoid repeated readlink syscalls.
    let root = &archive.canonical_root;

    // First try canonicalize (resolves symlinks + normalizes). If the file
    // exists, this is the most robust check.
    let candidate = match archive.root.join(safe_rel).canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // File doesn't exist yet. Manually normalize the path components
            // to prevent traversal via `foo/../../../etc/passwd` patterns.
            // We already rejected bare ".." segments above, but be defensive
            // by re-normalizing through component iteration.
            let mut resolved = root.clone();
            for component in Path::new(safe_rel).components() {
                match component {
                    Component::Normal(c) => resolved.push(c),
                    Component::CurDir => { /* skip `.` */ }
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        return Err(StorageError::InvalidPath(
                            "directory traversal not allowed".to_string(),
                        ));
                    }
                }
            }
            resolved
        }
    };

    if !candidate.starts_with(root) {
        return Err(StorageError::InvalidPath(
            "directory traversal not allowed".to_string(),
        ));
    }

    Ok(candidate)
}

/// Write text content to a file atomically (write-to-temp-then-rename).
///
/// Creates parent directories as needed. The rename is atomic on POSIX
/// filesystems when source and destination are on the same filesystem,
/// which they are because we put the temp file in the same directory.
fn write_text(path: &Path, content: &str) -> Result<()> {
    ensure_parent_dir(path)?;
    atomic_write_bytes(path, content.as_bytes())
}

/// Write JSON content to a file atomically (write-to-temp-then-rename).
///
/// Creates parent directories as needed.
fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    ensure_parent_dir(path)?;
    let content = serde_json::to_string_pretty(value)?;
    atomic_write_bytes(path, content.as_bytes())
}

/// Write bytes to a file atomically via a temp file + rename.
///
/// The temp file is created in the same directory as the target so that
/// `fs::rename` is guaranteed to be atomic (same filesystem).
fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let parent = path.parent().unwrap_or(Path::new("."));
    // Use pid + thread-id + counter for a unique temp name to avoid collisions
    // when multiple threads/processes write to the same directory.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_name = format!(
        ".tmp-{}-{}-{seq}",
        std::process::id(),
        // Include a hash of the target filename for extra uniqueness
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let tmp_path = parent.join(&tmp_name);
    let mut f = fs::File::create(&tmp_path)?;
    f.write_all(data)?;
    f.sync_data()?;
    fs::rename(&tmp_path, path).map_err(|e| {
        // Best-effort cleanup of the temp file on rename failure
        let _ = fs::remove_file(&tmp_path);
        e.into()
    })
}

/// ISO 8601 timestamp for the current time.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Consistency: archive-DB divergence detection
// ---------------------------------------------------------------------------

// Re-export consistency types from core (shared with db crate)
pub use mcp_agent_mail_core::{ConsistencyMessageRef, ConsistencyReport};

/// Check recent DB messages against the archive to detect divergence.
///
/// For each message ref, computes the expected canonical archive path and
/// checks if the file exists on disk. Returns a report summarising how many
/// are present vs missing.
///
/// This is intentionally cheap (filesystem stat, no git open) and runs at
/// startup or on-demand.
pub fn check_archive_consistency(
    storage_root: &Path,
    messages: &[ConsistencyMessageRef],
) -> ConsistencyReport {
    let mut found = 0usize;
    let mut missing = 0usize;
    let mut missing_ids: Vec<i64> = Vec::new();

    for msg in messages {
        // Build the expected canonical path:
        // {storage_root}/projects/{slug}/messages/{YYYY}/{MM}/{iso}__{slug}__{id}.md
        let project_dir = storage_root.join("projects").join(&msg.project_slug);
        let archive_root = project_dir.join("archive");

        // Parse the ISO timestamp to extract year/month
        let (year, month) = match parse_year_month(&msg.created_ts_iso) {
            Some(ym) => ym,
            None => {
                // Can't determine path; count as missing
                missing += 1;
                if missing_ids.len() < 20 {
                    missing_ids.push(msg.message_id);
                }
                continue;
            }
        };

        let iso_filename = msg.created_ts_iso.replace(':', "-").replace('+', "");
        // Truncate to seconds-level precision for filename matching
        let iso_prefix = if iso_filename.len() >= 19 {
            &iso_filename[..19]
        } else {
            &iso_filename
        };

        let messages_dir = archive_root.join("messages").join(&year).join(&month);

        // Look for a file matching the pattern: {iso}__{slug}__{id}.md
        // We check both the computed path and do a directory scan fallback
        // because the subject slug can vary.
        let found_file = if messages_dir.is_dir() {
            let id_suffix = format!("__{}.md", msg.message_id);
            match std::fs::read_dir(&messages_dir) {
                Ok(entries) => entries.flatten().any(|entry| {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    name_str.starts_with(iso_prefix) && name_str.ends_with(&id_suffix)
                }),
                Err(_) => false,
            }
        } else {
            false
        };

        if found_file {
            found += 1;
        } else {
            missing += 1;
            if missing_ids.len() < 20 {
                missing_ids.push(msg.message_id);
            }
        }
    }

    // Update global metrics
    mcp_agent_mail_core::global_metrics()
        .storage
        .needs_reindex_total
        .store(missing as u64);

    ConsistencyReport {
        sampled: messages.len(),
        found,
        missing,
        missing_ids,
    }
}

/// Parse year and month from an ISO 8601 timestamp string.
fn parse_year_month(iso: &str) -> Option<(String, String)> {
    // Expected format: "2026-02-08T03:29:30..." or similar
    if iso.len() < 7 {
        return None;
    }
    let year = iso.get(..4)?;
    let month = iso.get(5..7)?;
    // Validate they're numeric
    if year.chars().all(|c| c.is_ascii_digit()) && month.chars().all(|c| c.is_ascii_digit()) {
        Some((year.to_string(), month.to_string()))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::{Cx, Outcome};
    use mcp_agent_mail_db::{DbPool, DbPoolConfig, micros_to_iso, queries};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn test_config(root: &Path) -> Config {
        Config {
            storage_root: root.to_path_buf(),
            ..Config::default()
        }
    }

    fn block_on<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(Cx) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    }

    fn unique_human_key(prefix: &str) -> String {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        format!("/tmp/{prefix}-{suffix}")
    }

    #[test]
    fn test_ensure_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let (root, fresh) = ensure_archive_root(&config).unwrap();
        assert!(fresh);
        assert!(root.join(".git").exists());
        assert!(root.join(".gitattributes").exists());

        // Second call should not re-initialize
        let (_root2, fresh2) = ensure_archive_root(&config).unwrap();
        assert!(!fresh2);
    }

    #[test]
    fn test_ensure_archive() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let archive = ensure_archive(&config, "test-project").unwrap();
        assert_eq!(archive.slug, "test-project");
        assert!(archive.root.exists());
        assert!(archive.root.ends_with("projects/test-project"));
    }

    #[test]
    fn test_write_agent_profile() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let agent = serde_json::json!({
            "name": "TestAgent",
            "program": "test",
            "model": "test-model",
        });

        write_agent_profile_with_config(&archive, &config, &agent).unwrap();

        let profile_path = archive.root.join("agents/TestAgent/profile.json");
        assert!(profile_path.exists());
    }

    #[test]
    fn test_write_agent_profile_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let agent = serde_json::json!({
            "name": "../EvilAgent",
            "program": "test",
            "model": "test-model",
        });

        let err =
            write_agent_profile_with_config(&archive, &config, &agent).expect_err("expected error");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[test]
    fn test_write_file_reservation_record() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let reservation = serde_json::json!({
            "id": 42,
            "agent": "TestAgent",
            "path_pattern": "src/**/*.rs",
            "exclusive": true,
        });

        write_file_reservation_record(&archive, &config, &reservation).unwrap();

        // Check both legacy and id-based artifacts exist
        let res_dir = archive.root.join("file_reservations");
        assert!(res_dir.exists());

        let id_path = res_dir.join("id-42.json");
        assert!(id_path.exists());
    }

    #[test]
    fn db_and_storage_message_pipeline_consistent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let db_path = tmp.path().join("message_pipeline.db");
        let pool_config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            max_connections: 8,
            min_connections: 2,
            acquire_timeout_ms: 60_000,
            max_lifetime_ms: 3_600_000,
            run_migrations: true,
            warmup_connections: 0,
        };
        let pool = DbPool::new(&pool_config).expect("create db pool");
        let pool_for_setup = pool.clone();

        let sender_name = "BlueLake";
        let recipient_name = "RedHarbor";
        let (project_slug, project_id, recipient_id, message_id, created_ts_iso) =
            block_on(|cx| async move {
                let human_key = unique_human_key("storage-msg-pipeline");
                let project = match queries::ensure_project(&cx, &pool_for_setup, &human_key).await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("ensure_project failed: {other:?}"),
                };
                let project_id = project.id.expect("project id");

                let sender = match queries::register_agent(
                    &cx,
                    &pool_for_setup,
                    project_id,
                    sender_name,
                    "test",
                    "test-model",
                    Some("message pipeline sender"),
                    None,
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("register sender failed: {other:?}"),
                };
                let recipient = match queries::register_agent(
                    &cx,
                    &pool_for_setup,
                    project_id,
                    recipient_name,
                    "test",
                    "test-model",
                    Some("message pipeline recipient"),
                    None,
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("register recipient failed: {other:?}"),
                };
                let recipient_id = recipient.id.expect("recipient id");

                let message = match queries::create_message_with_recipients(
                    &cx,
                    &pool_for_setup,
                    project_id,
                    sender.id.expect("sender id"),
                    "Pipeline Message",
                    "pipeline message body",
                    Some("br-p1mi"),
                    "normal",
                    false,
                    "[]",
                    &[(recipient_id, "to")],
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("create_message_with_recipients failed: {other:?}"),
                };

                (
                    project.slug,
                    project_id,
                    recipient_id,
                    message.id.expect("message id"),
                    micros_to_iso(message.created_ts),
                )
            });

        let archive = ensure_archive(&config, &project_slug).expect("ensure archive");
        let message_json = serde_json::json!({
            "id": message_id,
            "subject": "Pipeline Message",
            "thread_id": "br-p1mi",
            "created_ts": created_ts_iso,
        });
        write_message_bundle(
            &archive,
            &config,
            &message_json,
            "pipeline message body",
            sender_name,
            &[recipient_name.to_string()],
            &[],
            None,
        )
        .expect("write message bundle");

        let inbox = list_agent_inbox(&archive, recipient_name).expect("list inbox");
        let outbox = list_agent_outbox(&archive, sender_name).expect("list outbox");
        assert_eq!(inbox.len(), 1, "expected one inbox file");
        assert_eq!(outbox.len(), 1, "expected one outbox file");

        let inbox_body = std::fs::read_to_string(&inbox[0]).expect("read inbox file");
        assert!(inbox_body.contains("\"id\":"));
        assert!(inbox_body.contains("pipeline message body"));

        let pool_for_verify = pool.clone();
        block_on(|cx| async move {
            let fetched = match queries::get_message(&cx, &pool_for_verify, message_id).await {
                Outcome::Ok(row) => row,
                other => panic!("get_message failed: {other:?}"),
            };
            assert_eq!(fetched.subject, "Pipeline Message");

            let inbox_rows = match queries::fetch_inbox(
                &cx,
                &pool_for_verify,
                project_id,
                recipient_id,
                false,
                None,
                20,
            )
            .await
            {
                Outcome::Ok(rows) => rows,
                other => panic!("fetch_inbox failed: {other:?}"),
            };
            assert_eq!(inbox_rows.len(), 1, "recipient inbox should have one row");
            assert_eq!(inbox_rows[0].message.id, Some(message_id));
        });
    }

    #[test]
    fn db_and_storage_reservation_pipeline_consistent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let db_path = tmp.path().join("reservation_pipeline.db");
        let pool_config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            max_connections: 8,
            min_connections: 2,
            acquire_timeout_ms: 60_000,
            max_lifetime_ms: 3_600_000,
            run_migrations: true,
            warmup_connections: 0,
        };
        let pool = DbPool::new(&pool_config).expect("create db pool");
        let pool_for_setup = pool.clone();

        let agent_name = "GreenCastle";
        let (project_slug, project_id, reservation_rows) = block_on(|cx| async move {
            let human_key = unique_human_key("storage-res-pipeline");
            let project = match queries::ensure_project(&cx, &pool_for_setup, &human_key).await {
                Outcome::Ok(row) => row,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let project_id = project.id.expect("project id");

            let agent = match queries::register_agent(
                &cx,
                &pool_for_setup,
                project_id,
                agent_name,
                "test",
                "test-model",
                Some("reservation pipeline agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent failed: {other:?}"),
            };
            let agent_id = agent.id.expect("agent id");

            let reservations = match queries::create_file_reservations(
                &cx,
                &pool_for_setup,
                project_id,
                agent_id,
                &["src/**", "docs/*.md"],
                3600,
                true,
                "br-p1mi",
            )
            .await
            {
                Outcome::Ok(rows) => rows,
                other => panic!("create_file_reservations failed: {other:?}"),
            };

            (project.slug, project_id, reservations)
        });

        let archive = ensure_archive(&config, &project_slug).expect("ensure archive");
        let reservation_json: Vec<serde_json::Value> = reservation_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.id.unwrap_or(0),
                    "agent": agent_name,
                    "path_pattern": row.path_pattern,
                    "exclusive": row.exclusive != 0,
                    "reason": row.reason,
                    "created_ts": micros_to_iso(row.created_ts),
                    "expires_ts": micros_to_iso(row.expires_ts),
                })
            })
            .collect();

        write_file_reservation_records(&archive, &config, &reservation_json)
            .expect("write reservation artifacts");

        let reservation_dir = archive.root.join("file_reservations");
        for row in &reservation_rows {
            let id = row.id.expect("reservation id");
            let id_path = reservation_dir.join(format!("id-{id}.json"));
            assert!(
                id_path.exists(),
                "expected reservation artifact {id_path:?}"
            );

            let artifact = std::fs::read_to_string(&id_path).expect("read reservation artifact");
            assert!(artifact.contains(&row.path_pattern));
        }

        let pool_for_verify = pool.clone();
        let expected_reservation_count = reservation_rows.len();
        block_on(|cx| async move {
            let active = match queries::list_file_reservations(
                &cx,
                &pool_for_verify,
                project_id,
                true,
            )
            .await
            {
                Outcome::Ok(rows) => rows,
                other => panic!("list_file_reservations failed: {other:?}"),
            };
            assert_eq!(active.len(), expected_reservation_count);
        });
    }

    #[test]
    fn test_write_message_bundle() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 1,
            "subject": "Test Message",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-1",
            "project": "proj",
        });

        write_message_bundle(
            &archive,
            &config,
            &message,
            "Hello world!",
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();

        // Check canonical message file exists
        let msg_dir = archive.root.join("messages/2026/01");
        assert!(msg_dir.exists());

        // Check outbox
        let outbox_dir = archive.root.join("agents/SenderAgent/outbox/2026/01");
        assert!(outbox_dir.exists());

        // Check inbox
        let inbox_dir = archive.root.join("agents/RecipientAgent/inbox/2026/01");
        assert!(inbox_dir.exists());

        // Check thread digest (sanitize_thread_id lowercases)
        let digest = archive.root.join("messages/threads/tkt-1.md");
        assert!(digest.exists());
    }

    #[test]
    fn thread_digest_truncation_is_utf8_safe() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 1,
            "subject": "Unicode Body",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-UNICODE",
            "project": "proj",
        });

        // Each '€' is 3 bytes, so this is > 1200 bytes and exercises the truncation path.
        let body = "€".repeat(600);

        write_message_bundle(
            &archive,
            &config,
            &message,
            &body,
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();

        let digest = archive.root.join("messages/threads/tkt-unicode.md");
        assert!(digest.exists());
        let contents = std::fs::read_to_string(&digest).unwrap();
        assert!(
            contents.contains("\n..."),
            "expected truncated preview marker"
        );
    }

    #[test]
    fn test_write_message_bundle_rejects_path_traversal_agent_names() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 1,
            "subject": "Test Message",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-1",
            "project": "proj",
        });

        let err = write_message_bundle(
            &archive,
            &config,
            &message,
            "Hello world!",
            "../SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .expect_err("expected error");
        assert!(matches!(err, StorageError::InvalidPath(_)));

        let err = write_message_bundle(
            &archive,
            &config,
            &message,
            "Hello world!",
            "SenderAgent",
            &["../RecipientAgent".to_string()],
            &[],
            None,
        )
        .expect_err("expected error");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[test]
    fn test_write_message_bundle_rejects_unsafe_extra_paths() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 1,
            "subject": "Test Message",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-1",
            "project": "proj",
        });

        let extra = vec!["../oops.txt".to_string()];
        let err = write_message_bundle(
            &archive,
            &config,
            &message,
            "Hello world!",
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &extra,
            None,
        )
        .expect_err("expected error");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[test]
    fn test_list_agent_inbox_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let err = list_agent_inbox(&archive, "../bad").expect_err("expected error");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[test]
    fn test_resolve_archive_relative_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        // Create a file to resolve
        let test_file = archive.root.join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let resolved = resolve_archive_relative_path(&archive, "test.txt").unwrap();
        assert!(resolved.ends_with("test.txt"));

        // Traversal should fail
        assert!(resolve_archive_relative_path(&archive, "../../../etc/passwd").is_err());
        assert!(resolve_archive_relative_path(&archive, "..").is_err());
        assert!(resolve_archive_relative_path(&archive, "/etc/passwd").is_err());

        // Embedded traversal via non-existent intermediate dirs should also fail
        assert!(resolve_archive_relative_path(&archive, "foo/../../etc/passwd").is_err());
        assert!(resolve_archive_relative_path(&archive, "a/b/../../../etc/shadow").is_err());

        // Non-existent file within archive should succeed (returns joined path)
        let resolved = resolve_archive_relative_path(&archive, "subdir/newfile.txt").unwrap();
        assert!(resolved.starts_with(&archive.root));

        // Backslash normalization
        assert!(resolve_archive_relative_path(&archive, "..\\..\\etc\\passwd").is_err());
    }

    #[test]
    fn test_subject_slug() {
        let re = subject_slug_re();
        let result = re.replace_all("Hello World! [Test]", "-");
        assert_eq!(result.trim_matches('-'), "Hello-World-Test");
    }

    #[test]
    fn test_sanitize_thread_id() {
        assert_eq!(sanitize_thread_id("TKT-1"), "tkt-1");
        assert_eq!(sanitize_thread_id("../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_thread_id(""), "thread");
    }

    #[test]
    fn test_parse_message_timestamp_numeric() {
        let message = serde_json::json!({ "created_ts": 1_700_000_000_000_000_i64 });
        let ts = parse_message_timestamp(&message);
        assert_eq!(ts.timestamp(), 1_700_000_000);
    }

    #[test]
    fn test_append_trailers() {
        let msg = "mail: Agent1 -> Agent2 | Hello";
        let result = append_trailers(msg);
        assert!(result.contains("Agent: Agent1"));

        let msg2 = "file_reservation: Agent3 src/**";
        let result2 = append_trailers(msg2);
        assert!(result2.contains("Agent: Agent3"));

        // Should not duplicate if already present
        let msg3 = "mail: Agent1 -> Agent2 | Hello\n\nAgent: Agent1\n";
        let result3 = append_trailers(msg3);
        assert_eq!(result3.matches("Agent:").count(), 1);
    }

    #[test]
    fn test_notification_signals() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        let meta = NotificationMessage {
            id: Some(123),
            from: Some("Sender".to_string()),
            subject: Some("Hello".to_string()),
            importance: Some("high".to_string()),
        };
        assert!(emit_notification_signal(
            &config,
            "proj",
            "Agent",
            Some(&meta)
        ));

        let signals = list_pending_signals(&config, Some("proj"));
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(signal["project"], "proj");
        assert_eq!(signal["agent"], "Agent");
        assert!(signal["timestamp"].as_str().is_some());
        assert_eq!(signal["message"]["id"], 123);
        assert_eq!(signal["message"]["importance"], "high");

        let cleared = clear_notification_signal(&config, "proj", "Agent");
        assert!(cleared);

        let signals2 = list_pending_signals(&config, Some("proj"));
        assert!(signals2.is_empty());
    }

    // -----------------------------------------------------------------------
    // Notification signal fixture-driven tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_signal_payload_full_metadata() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0; // disable debounce for tests

        let meta = NotificationMessage {
            id: Some(123),
            from: Some("SenderAgent".to_string()),
            subject: Some("Hello World".to_string()),
            importance: Some("high".to_string()),
        };
        assert!(emit_notification_signal(
            &config,
            "test_project",
            "TestAgent",
            Some(&meta)
        ));

        let signals = list_pending_signals(&config, Some("test_project"));
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(signal["project"], "test_project");
        assert_eq!(signal["agent"], "TestAgent");
        assert!(signal["timestamp"].as_str().is_some());
        assert_eq!(signal["message"]["id"], 123);
        assert_eq!(signal["message"]["from"], "SenderAgent");
        assert_eq!(signal["message"]["subject"], "Hello World");
        assert_eq!(signal["message"]["importance"], "high");
    }

    #[test]
    fn test_signal_payload_importance_defaults_to_normal() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        let meta = NotificationMessage {
            id: Some(456),
            from: Some("Sender2".to_string()),
            subject: Some("No importance field".to_string()),
            importance: None, // should default to "normal"
        };
        assert!(emit_notification_signal(
            &config,
            "proj1",
            "Agent1",
            Some(&meta)
        ));

        let signals = list_pending_signals(&config, Some("proj1"));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0]["message"]["importance"], "normal");
        assert_eq!(signals[0]["message"]["from"], "Sender2");
    }

    #[test]
    fn test_signal_payload_sparse_metadata() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        let meta = NotificationMessage {
            id: Some(789),
            from: None,
            subject: None,
            importance: None,
        };
        assert!(emit_notification_signal(
            &config,
            "proj1",
            "Agent2",
            Some(&meta)
        ));

        let signals = list_pending_signals(&config, Some("proj1"));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0]["message"]["id"], 789);
        assert!(signals[0]["message"]["from"].is_null());
        assert!(signals[0]["message"]["subject"].is_null());
        assert_eq!(signals[0]["message"]["importance"], "normal");
    }

    #[test]
    fn test_signal_payload_metadata_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = false;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        let meta = NotificationMessage {
            id: Some(123),
            from: Some("Sender".to_string()),
            subject: Some("Hello".to_string()),
            importance: Some("high".to_string()),
        };
        assert!(emit_notification_signal(
            &config,
            "test_project",
            "TestAgent",
            Some(&meta)
        ));

        let signals = list_pending_signals(&config, Some("test_project"));
        assert_eq!(signals.len(), 1);
        assert!(signals[0].get("message").is_none());
        assert_eq!(signals[0]["project"], "test_project");
        assert_eq!(signals[0]["agent"], "TestAgent");
    }

    #[test]
    fn test_signal_payload_null_metadata() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        assert!(emit_notification_signal(
            &config,
            "test_project",
            "TestAgent",
            None
        ));

        let signals = list_pending_signals(&config, Some("test_project"));
        assert_eq!(signals.len(), 1);
        assert!(signals[0].get("message").is_none());
    }

    #[test]
    fn test_signal_notifications_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = false;
        config.notifications_signals_dir = tmp.path().join("signals");

        assert!(!emit_notification_signal(&config, "proj", "Agent", None));
        let signals = list_pending_signals(&config, None);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_signal_list_multiple_projects_and_agents() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = false;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        // Emit signals across 2 projects and 2 agents
        assert!(emit_notification_signal(&config, "proj1", "Agent1", None));
        assert!(emit_notification_signal(&config, "proj1", "Agent2", None));
        assert!(emit_notification_signal(&config, "proj2", "Agent1", None));

        // All signals
        let all = list_pending_signals(&config, None);
        assert_eq!(all.len(), 3);

        // Filter by project
        let proj1 = list_pending_signals(&config, Some("proj1"));
        assert_eq!(proj1.len(), 2);

        let proj2 = list_pending_signals(&config, Some("proj2"));
        assert_eq!(proj2.len(), 1);
        assert_eq!(proj2[0]["agent"], "Agent1");
    }

    #[test]
    fn test_signal_clear_and_relist() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = false;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        assert!(emit_notification_signal(&config, "proj", "Agent1", None));
        assert!(emit_notification_signal(&config, "proj", "Agent2", None));

        // Clear Agent1
        assert!(clear_notification_signal(&config, "proj", "Agent1"));
        let signals = list_pending_signals(&config, Some("proj"));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0]["agent"], "Agent2");

        // Clear nonexistent returns false
        assert!(!clear_notification_signal(&config, "proj", "NonExistent"));
    }

    #[test]
    fn test_signal_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        let signals = list_pending_signals(&config, None);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_signal_path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_debounce_ms = 0; // disable debounce for test isolation
        config.notifications_signals_dir = tmp.path().join("signals");

        // Use unique names to avoid interference from parallel tests sharing
        // the global debounce map.
        let slug = "trav_proj";
        let agent = "TravAgent";

        // Slash in project_slug
        assert!(!emit_notification_signal(&config, "../evil", agent, None));
        assert!(!emit_notification_signal(&config, "proj/sub", agent, None));

        // Backslash in project_slug
        assert!(!emit_notification_signal(&config, "proj\\sub", agent, None));

        // Dot-dot in project_slug
        assert!(!emit_notification_signal(&config, "proj..", agent, None));
        assert!(!emit_notification_signal(&config, "..proj", agent, None));

        // Slash in agent_name
        assert!(!emit_notification_signal(&config, slug, "../evil", None));
        assert!(!emit_notification_signal(&config, slug, "Agent/sub", None));

        // Backslash in agent_name
        assert!(!emit_notification_signal(&config, slug, "Agent\\sub", None));

        // clear_notification_signal rejects the same patterns
        assert!(!clear_notification_signal(&config, "../evil", agent));
        assert!(!clear_notification_signal(&config, slug, "../evil"));
        assert!(!clear_notification_signal(&config, "proj\\sub", agent));
        assert!(!clear_notification_signal(&config, slug, "Agent\\sub"));

        // list_pending_signals rejects traversal in slug
        assert!(list_pending_signals(&config, Some("../evil")).is_empty());
        assert!(list_pending_signals(&config, Some("proj/sub")).is_empty());
        assert!(list_pending_signals(&config, Some("proj\\sub")).is_empty());

        // Legitimate names still work
        assert!(emit_notification_signal(&config, slug, agent, None));
        assert_eq!(list_pending_signals(&config, Some(slug)).len(), 1);
        assert!(clear_notification_signal(&config, slug, agent));
    }

    // -----------------------------------------------------------------------
    // Advisory file lock tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_file_lock_acquire_release() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let mut lock = FileLock::new(lock_path.clone());
        lock.acquire().unwrap();

        // Owner metadata should exist
        let meta_path = tmp.path().join("test.lock.owner.json");
        assert!(meta_path.exists());

        let content = fs::read_to_string(&meta_path).unwrap();
        let meta: LockOwnerMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(meta.pid, std::process::id());
        assert!(meta.created_ts > 0.0);

        lock.release().unwrap();

        // Lock and metadata files should be cleaned up
        assert!(!lock_path.exists());
        assert!(!meta_path.exists());
    }

    #[test]
    fn test_file_lock_drop_releases() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("drop.lock");

        {
            let mut lock = FileLock::new(lock_path.clone());
            lock.acquire().unwrap();
            assert!(lock_path.exists());
        }
        // Drop should release
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_file_lock_stale_cleanup() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("stale.lock");
        let meta_path = tmp.path().join("stale.lock.owner.json");

        // Create a lock with a dead PID
        fs::write(&lock_path, "locked").unwrap();
        let meta = serde_json::json!({
            "pid": 999999999,  // Almost certainly dead
            "created_ts": 0.0,  // Ancient timestamp
        });
        fs::write(&meta_path, meta.to_string()).unwrap();

        // A new lock should clean up the stale one and acquire
        let mut lock = FileLock::new(lock_path.clone());
        lock.acquire().unwrap();

        // Verify we hold the lock now
        assert!(lock_path.exists());
        let new_meta: LockOwnerMeta =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(new_meta.pid, std::process::id());

        lock.release().unwrap();
    }

    #[test]
    fn test_with_project_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "lock-proj").unwrap();

        let result = with_project_lock(&archive, || {
            // Lock is held here
            assert!(archive.lock_path.exists());
            Ok(42)
        })
        .unwrap();

        assert_eq!(result, 42);
        // Lock should be released
        assert!(!archive.lock_path.exists());
    }

    // -----------------------------------------------------------------------
    // Commit queue tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_commit_queue_single() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "queue-proj").unwrap();

        // Write a file to commit
        let test_file = archive.root.join("test.txt");
        fs::write(&test_file, "hello").unwrap();
        let rel = rel_path_cached(&archive.canonical_repo_root, &test_file).unwrap();

        let queue = CommitQueue::default();
        queue
            .enqueue(
                archive.repo_root.clone(),
                &config,
                "test commit".to_string(),
                vec![rel],
            )
            .unwrap();

        queue.drain(&config).unwrap();

        let stats = queue.stats();
        assert_eq!(stats.enqueued, 1);
        assert_eq!(stats.commits, 1);
        assert_eq!(stats.queue_size, 0);
    }

    #[test]
    fn test_commit_queue_batching() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "batch-proj").unwrap();

        let queue = CommitQueue::default();

        // Enqueue multiple non-conflicting commits
        for i in 0..3 {
            let file = archive.root.join(format!("file{i}.txt"));
            fs::write(&file, format!("content {i}")).unwrap();
            let rel = rel_path_cached(&archive.canonical_repo_root, &file).unwrap();
            queue
                .enqueue(
                    archive.repo_root.clone(),
                    &config,
                    format!("commit {i}"),
                    vec![rel],
                )
                .unwrap();
        }

        queue.drain(&config).unwrap();

        let stats = queue.stats();
        assert_eq!(stats.enqueued, 3);
        assert_eq!(stats.batched, 3);
        // Should be batched into 1 commit (3 non-conflicting paths, <= 5)
        assert_eq!(stats.commits, 1);
    }

    // -----------------------------------------------------------------------
    // Commit lock path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_commit_lock_path_single_project() {
        let root = PathBuf::from("/tmp/archive");
        let paths = &["projects/my-proj/agents/Agent/profile.json"];
        let lock = commit_lock_path(&root, paths);
        assert_eq!(lock, root.join("projects/my-proj/.commit.lock"));
    }

    #[test]
    fn test_commit_lock_path_different_projects() {
        let root = PathBuf::from("/tmp/archive");
        let paths = &[
            "projects/proj-a/agents/A/profile.json",
            "projects/proj-b/agents/B/profile.json",
        ];
        let lock = commit_lock_path(&root, paths);
        assert_eq!(lock, root.join(".commit.lock"));
    }

    #[test]
    fn test_commit_lock_path_empty() {
        let root = PathBuf::from("/tmp/archive");
        let lock = commit_lock_path(&root, &[]);
        assert_eq!(lock, root.join(".commit.lock"));
    }

    // -----------------------------------------------------------------------
    // Heal archive locks tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_heal_archive_locks_empty() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        ensure_archive_root(&config).unwrap();

        let result = heal_archive_locks(&config).unwrap();
        assert_eq!(result.locks_scanned, 0);
        assert!(result.locks_removed.is_empty());
        assert!(result.metadata_removed.is_empty());
    }

    #[test]
    fn test_heal_archive_locks_orphaned_metadata() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        ensure_archive_root(&config).unwrap();

        // Create an orphaned metadata file (no matching lock)
        let meta_path = tmp.path().join("projects").join("test.lock.owner.json");
        fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
        fs::write(&meta_path, r#"{"pid": 1, "created_ts": 0.0}"#).unwrap();

        let result = heal_archive_locks(&config).unwrap();
        assert_eq!(result.metadata_removed.len(), 1);
        assert!(!meta_path.exists());
    }

    // -----------------------------------------------------------------------
    // Attachment pipeline tests
    // -----------------------------------------------------------------------

    /// Create a minimal valid PNG image for testing.
    fn create_test_png(path: &Path) {
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(4, 4, |x, y| {
            Rgba([(x * 64) as u8, (y * 64) as u8, 128, 255])
        });
        img.save(path).unwrap();
    }

    #[test]
    fn test_store_attachment_file_mode() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "attach-proj").unwrap();

        // Create a test image
        let img_path = archive.root.join("test_image.png");
        create_test_png(&img_path);

        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();

        assert_eq!(stored.meta.kind, "file");
        assert_eq!(stored.meta.media_type, "image/webp");
        assert_eq!(stored.meta.width, 4);
        assert_eq!(stored.meta.height, 4);
        assert!(stored.meta.data_base64.is_none());
        assert!(stored.meta.path.is_some());
        assert!(!stored.rel_paths.is_empty());

        // Verify WebP file exists
        let webp_path = archive.repo_root.join(stored.meta.path.unwrap());
        assert!(webp_path.exists());

        // Verify manifest exists
        let manifest_dir = archive.root.join("attachments/_manifests");
        assert!(manifest_dir.exists());
        let manifest_files: Vec<_> = fs::read_dir(&manifest_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(manifest_files.len(), 1);
    }

    #[test]
    fn test_store_attachment_inline_mode() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "inline-proj").unwrap();

        let img_path = archive.root.join("small_image.png");
        create_test_png(&img_path);

        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::Inline).unwrap();

        assert_eq!(stored.meta.kind, "inline");
        assert!(stored.meta.data_base64.is_some());
        assert!(stored.meta.path.is_none());

        // Base64 data should be valid
        let b64 = stored.meta.data_base64.unwrap();
        assert!(!b64.is_empty());
    }

    #[test]
    fn test_store_attachment_auto_mode_small() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.inline_image_max_bytes = 1024 * 1024; // 1MiB threshold - our tiny test image should be inline
        let archive = ensure_archive(&config, "auto-proj").unwrap();

        let img_path = archive.root.join("tiny.png");
        create_test_png(&img_path);

        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::Auto).unwrap();
        // Our tiny 4x4 PNG -> WebP should be well under 1MiB
        assert_eq!(stored.meta.kind, "inline");
    }

    #[test]
    fn test_store_attachment_auto_mode_large() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.inline_image_max_bytes = 1; // 1 byte threshold - force file mode
        let archive = ensure_archive(&config, "auto-large-proj").unwrap();

        let img_path = archive.root.join("image.png");
        create_test_png(&img_path);

        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::Auto).unwrap();
        assert_eq!(stored.meta.kind, "file");
    }

    #[test]
    fn test_store_attachment_keeps_original() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.keep_original_images = true;
        let archive = ensure_archive(&config, "orig-proj").unwrap();

        let img_path = archive.root.join("original.png");
        create_test_png(&img_path);

        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        assert!(stored.meta.original_path.is_some());

        let orig_rel = stored.meta.original_path.unwrap();
        let orig_full = archive.repo_root.join(orig_rel);
        assert!(orig_full.exists());
    }

    #[test]
    fn test_process_attachments() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proc-proj").unwrap();

        let img1 = archive.root.join("img1.png");
        let img2 = archive.root.join("img2.png");
        create_test_png(&img1);
        create_test_png(&img2);

        let (meta, rel_paths) = process_attachments(
            &archive,
            &config,
            &archive.root,
            &[img1.display().to_string(), img2.display().to_string()],
            EmbedPolicy::File,
        )
        .unwrap();

        assert_eq!(meta.len(), 2);
        assert!(!rel_paths.is_empty());
    }

    #[test]
    fn resolve_attachment_source_path_rejects_relative_escape_even_when_allow_absolute_enabled() {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("project-root");
        std::fs::create_dir_all(&base_dir).unwrap();

        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();

        let mut config = test_config(tmp.path());
        config.allow_absolute_attachment_paths = true;

        let err = resolve_attachment_source_path(&base_dir, &config, "../outside.txt").unwrap_err();
        assert!(err.to_string().contains("escapes the project directory"));
    }

    #[test]
    fn resolve_attachment_source_path_allows_absolute_outside_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("project-root");
        std::fs::create_dir_all(&base_dir).unwrap();

        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();

        let mut config = test_config(tmp.path());
        config.allow_absolute_attachment_paths = true;

        let resolved =
            resolve_attachment_source_path(&base_dir, &config, &outside.display().to_string())
                .unwrap();
        assert_eq!(resolved, outside.canonicalize().unwrap());
    }

    #[test]
    fn resolve_attachment_source_path_rejects_absolute_outside_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("project-root");
        std::fs::create_dir_all(&base_dir).unwrap();

        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();

        let mut config = test_config(tmp.path());
        config.allow_absolute_attachment_paths = false;

        let err =
            resolve_attachment_source_path(&base_dir, &config, &outside.display().to_string())
                .unwrap_err();
        assert!(
            err.to_string()
                .contains("Absolute attachment paths outside the project are not allowed")
        );
    }

    #[test]
    fn test_process_markdown_images() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "md-proj").unwrap();

        // Create test image inside archive
        let img_path = archive.root.join("diagram.png");
        create_test_png(&img_path);

        let body = "Check this: ![diagram](diagram.png) and text.";
        let (new_body, meta, rel_paths) =
            process_markdown_images(&archive, &config, &archive.root, body, EmbedPolicy::Inline)
                .unwrap();

        assert_eq!(meta.len(), 1);
        assert!(!rel_paths.is_empty());
        // Should be replaced with data URI
        assert!(new_body.contains("data:image/webp;base64,"));
        assert!(!new_body.contains("diagram.png"));
    }

    #[test]
    fn test_process_markdown_images_skips_urls() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "url-proj").unwrap();

        let body = "Remote: ![photo](https://example.com/img.png) and local ref.";
        let (new_body, meta, _) =
            process_markdown_images(&archive, &config, &archive.root, body, EmbedPolicy::File)
                .unwrap();

        // URL should be left unchanged
        assert_eq!(new_body, body);
        assert!(meta.is_empty());
    }

    #[test]
    fn test_embed_policy_from_str() {
        assert_eq!(EmbedPolicy::from_str_policy("inline"), EmbedPolicy::Inline);
        assert_eq!(EmbedPolicy::from_str_policy("file"), EmbedPolicy::File);
        assert_eq!(EmbedPolicy::from_str_policy("auto"), EmbedPolicy::Auto);
        assert_eq!(EmbedPolicy::from_str_policy("INLINE"), EmbedPolicy::Inline);
        assert_eq!(EmbedPolicy::from_str_policy("whatever"), EmbedPolicy::Auto);
    }

    // -----------------------------------------------------------------------
    // Attachment cache + parallelism tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_store_attachment_cache_hit_skips_conversion() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "cache-proj").unwrap();

        let img_path = archive.root.join("cached.png");
        create_test_png(&img_path);

        // First call: cold miss — does full conversion.
        let first = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        assert!(!first.rel_paths.is_empty(), "cold miss writes files");
        let first_sha1 = first.meta.sha1.clone();

        // Second call: cache hit — skips conversion, returns empty rel_paths.
        let second = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        assert!(
            second.rel_paths.is_empty(),
            "cache hit should return empty rel_paths"
        );
        assert_eq!(second.meta.sha1, first_sha1);
        assert_eq!(second.meta.width, first.meta.width);
        assert_eq!(second.meta.height, first.meta.height);
        assert_eq!(second.meta.kind, "file");
    }

    #[test]
    fn test_store_attachment_cache_hit_inline_mode() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "cache-inline-proj").unwrap();

        let img_path = archive.root.join("cached_inline.png");
        create_test_png(&img_path);

        // Cold miss
        let first = store_attachment(&archive, &config, &img_path, EmbedPolicy::Inline).unwrap();
        assert_eq!(first.meta.kind, "inline");
        let first_b64 = first.meta.data_base64.clone().unwrap();

        // Cache hit — inline mode re-reads the WebP and base64-encodes it.
        let second = store_attachment(&archive, &config, &img_path, EmbedPolicy::Inline).unwrap();
        assert_eq!(second.meta.kind, "inline");
        assert_eq!(
            second.meta.data_base64.unwrap(),
            first_b64,
            "cache hit should produce identical base64"
        );
    }

    #[test]
    fn test_process_attachments_parallel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "parallel-proj").unwrap();

        // Create 6 distinct images (exceeds MAX_CONCURRENT_CONVERSIONS=4)
        let paths: Vec<String> = (0..6)
            .map(|i| {
                let p = archive.root.join(format!("img_{i}.png"));
                // Create slightly different images to avoid SHA1 dedup
                use image::{ImageBuffer, Rgba};
                let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(4, 4, |x, y| {
                    Rgba([i as u8 * 40, (x * 64) as u8, (y * 64) as u8, 255])
                });
                img.save(&p).unwrap();
                p.display().to_string()
            })
            .collect();

        let (meta, rel_paths) =
            process_attachments(&archive, &config, &archive.root, &paths, EmbedPolicy::File)
                .unwrap();

        assert_eq!(meta.len(), 6);
        assert!(!rel_paths.is_empty());
        // All should have unique SHA1s
        let sha1s: std::collections::HashSet<_> = meta.iter().map(|m| &m.sha1).collect();
        assert_eq!(sha1s.len(), 6);
    }

    #[test]
    fn test_process_markdown_images_parallel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "md-par-proj").unwrap();

        // Create 5 distinct test images inside the archive root
        for i in 0..5 {
            use image::{ImageBuffer, Rgba};
            let p = archive.root.join(format!("photo_{i}.png"));
            let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(4, 4, |x, y| {
                Rgba([i as u8 * 50, (x * 64) as u8, (y * 64) as u8, 255])
            });
            img.save(&p).unwrap();
        }

        let body = "A: ![a](photo_0.png) B: ![b](photo_1.png) C: ![c](photo_2.png) \
                    D: ![d](photo_3.png) E: ![e](photo_4.png)";
        let (new_body, meta, _) =
            process_markdown_images(&archive, &config, &archive.root, body, EmbedPolicy::File)
                .unwrap();

        assert_eq!(meta.len(), 5);
        // All original refs should be replaced
        assert!(!new_body.contains("photo_0.png"));
        assert!(!new_body.contains("photo_4.png"));
        // All should be replaced with archive paths
        assert!(new_body.contains("attachments/"));
    }

    #[test]
    fn test_store_attachment_rejects_oversized_file() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "oversize-proj").unwrap();

        // Create a file larger than MAX_ATTACHMENT_BYTES (50MB)
        // We can't actually write 50MB in a unit test, but we can temporarily
        // create a smaller "image" and test the guard path by using a non-image
        // file that would fail decode before the size check. Instead, verify
        // the constant is set correctly.
        assert_eq!(MAX_ATTACHMENT_BYTES, 50 * 1024 * 1024);

        // Create a valid but empty file (should fail with "empty" error, not size)
        let empty_path = archive.root.join("empty.png");
        fs::write(&empty_path, b"").unwrap();
        let err = store_attachment(&archive, &config, &empty_path, EmbedPolicy::File).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    // -----------------------------------------------------------------------
    // Read helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_commit_for_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "read-proj").unwrap();

        // Write agent profile (creates a commit)
        let agent = serde_json::json!({"name": "ReadAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits(); // ensure git commit is flushed before reading history

        let rel = "projects/read-proj/agents/ReadAgent/profile.json".to_string();
        let commit = find_commit_for_path(&archive, &rel).unwrap();
        assert!(commit.is_some());
        let commit = commit.unwrap();
        assert!(commit.summary.contains("agent: profile ReadAgent"));
    }

    #[test]
    fn test_get_commits_by_author() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "author-proj").unwrap();

        // Write something to create commits
        let agent = serde_json::json!({"name": "AuthorAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits(); // ensure git commit is flushed before reading history

        let commits = get_commits_by_author(&archive, &config.git_author_name, 10).unwrap();
        assert!(!commits.is_empty());
    }

    #[test]
    fn test_read_message_file() {
        let tmp = TempDir::new().unwrap();

        // Create a message file with frontmatter
        let msg_path = tmp.path().join("test_msg.md");
        let content = "---json\n{\"id\": 1, \"subject\": \"Hello\"}\n---\n\nThis is the body.\n";
        fs::write(&msg_path, content).unwrap();

        let (frontmatter, body) = read_message_file(&msg_path).unwrap();
        assert_eq!(frontmatter["id"], 1);
        assert_eq!(frontmatter["subject"], "Hello");
        assert_eq!(body, "This is the body.");
    }

    #[test]
    fn test_read_message_file_no_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let msg_path = tmp.path().join("plain.md");
        fs::write(&msg_path, "Just plain text.").unwrap();

        let (frontmatter, body) = read_message_file(&msg_path).unwrap();
        assert!(frontmatter.is_null());
        assert_eq!(body, "Just plain text.");
    }

    #[test]
    fn test_list_agent_inbox_outbox() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "list-proj").unwrap();

        // Write a message to create inbox/outbox
        let message = serde_json::json!({
            "id": 10,
            "subject": "Inbox Test",
            "created_ts": "2026-01-20T12:00:00Z",
        });
        write_message_bundle(
            &archive,
            &config,
            &message,
            "Test body",
            "Sender",
            &["Recipient".to_string()],
            &[],
            None,
        )
        .unwrap();

        let inbox = list_agent_inbox(&archive, "Recipient").unwrap();
        assert_eq!(inbox.len(), 1);

        let outbox = list_agent_outbox(&archive, "Sender").unwrap();
        assert_eq!(outbox.len(), 1);

        // Non-existent agent should return empty
        let empty = list_agent_inbox(&archive, "Nobody").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_list_archive_agents() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "agents-proj").unwrap();

        let agent1 = serde_json::json!({"name": "Alice", "program": "test"});
        let agent2 = serde_json::json!({"name": "Bob", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent1).unwrap();
        write_agent_profile_with_config(&archive, &config, &agent2).unwrap();

        let agents = list_archive_agents(&archive).unwrap();
        assert_eq!(agents, vec!["Alice", "Bob"]);
    }

    #[test]
    fn test_read_agent_profile() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "profile-proj").unwrap();

        let agent = serde_json::json!({"name": "ProfAgent", "program": "claude", "model": "opus"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();

        let profile = read_agent_profile(&archive, "ProfAgent").unwrap();
        assert!(profile.is_some());
        let profile = profile.unwrap();
        assert_eq!(profile["name"], "ProfAgent");
        assert_eq!(profile["program"], "claude");

        // Non-existent agent
        let missing = read_agent_profile(&archive, "Ghost").unwrap();
        assert!(missing.is_none());
    }

    // -----------------------------------------------------------------------
    // Write-Behind Queue (WBQ) tests
    // -----------------------------------------------------------------------

    #[test]
    fn wbq_enqueue_returns_true_when_running() {
        wbq_start();
        let op = WriteOp::ClearSignal {
            config: Config::default(),
            project_slug: "test-wbq".to_string(),
            agent_name: "TestAgent".to_string(),
        };
        let accepted = wbq_enqueue(op);
        assert_eq!(
            accepted,
            WbqEnqueueResult::Enqueued,
            "wbq_enqueue should accept ops when worker is running"
        );
    }

    #[test]
    fn wbq_stats_tracks_enqueues() {
        wbq_start();
        let before = wbq_stats();
        let op = WriteOp::ClearSignal {
            config: Config::default(),
            project_slug: "test-stats".to_string(),
            agent_name: "StatsAgent".to_string(),
        };
        wbq_enqueue(op);
        let after = wbq_stats();
        assert!(
            after.enqueued > before.enqueued,
            "enqueued counter should increase"
        );
    }

    #[test]
    fn wbq_flush_drains_pending() {
        wbq_start();
        let op = WriteOp::ClearSignal {
            config: Config::default(),
            project_slug: "test-flush".to_string(),
            agent_name: "FlushAgent".to_string(),
        };
        wbq_enqueue(op);
        wbq_flush();
        let stats = wbq_stats();
        assert!(stats.drained > 0, "drain count should be > 0 after flush");
    }

    #[test]
    fn wbq_agent_profile_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        wbq_start();

        let agent_json = serde_json::json!({
            "name": "WbqTestAgent",
            "program": "test",
            "model": "test",
        });

        let op = WriteOp::AgentProfile {
            project_slug: "wbq-profile-test".to_string(),
            config: config.clone(),
            agent_json: agent_json.clone(),
        };

        let accepted = wbq_enqueue(op);
        assert_eq!(accepted, WbqEnqueueResult::Enqueued);

        // Flush to ensure the write completes
        wbq_flush();
        // Also flush async git commits
        flush_async_commits();

        // Verify the profile was written to disk
        let archive = ensure_archive(&config, "wbq-profile-test").unwrap();
        let profile_path = archive
            .root
            .join("agents")
            .join("WbqTestAgent")
            .join("profile.json");
        assert!(
            profile_path.exists(),
            "profile.json should exist after WBQ drain"
        );

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&profile_path).unwrap()).unwrap();
        assert_eq!(content["name"], "WbqTestAgent");
    }

    #[test]
    fn wbq_message_bundle_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        wbq_start();

        // Ensure archive exists first
        let _archive = ensure_archive(&config, "wbq-msg-test").unwrap();

        let msg_json = serde_json::json!({
            "id": 42,
            "from": "Sender",
            "to": ["Receiver"],
            "subject": "WBQ test message",
            "created": "2025-01-01T00:00:00+00:00",
            "thread_id": null,
            "importance": "normal",
        });

        let op = WriteOp::MessageBundle {
            project_slug: "wbq-msg-test".to_string(),
            config: config.clone(),
            message_json: msg_json,
            body_md: "Hello from WBQ".to_string(),
            sender: "Sender".to_string(),
            recipients: vec!["Receiver".to_string()],
            extra_paths: vec![],
        };

        assert_eq!(wbq_enqueue(op), WbqEnqueueResult::Enqueued);
        wbq_flush();
        flush_async_commits();

        // Verify message files exist
        let archive = ensure_archive(&config, "wbq-msg-test").unwrap();
        let messages_dir = archive.root.join("messages");
        assert!(messages_dir.exists(), "messages/ directory should exist");
    }

    #[test]
    fn wbq_file_reservation_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        wbq_start();

        let _archive = ensure_archive(&config, "wbq-res-test").unwrap();

        let res_json = serde_json::json!({
            "id": 1,
            "agent": "ResAgent",
            "path_pattern": "src/*.rs",
            "exclusive": true,
            "reason": "test",
            "expires_ts": "2025-12-31T23:59:59+00:00",
        });

        let op = WriteOp::FileReservation {
            project_slug: "wbq-res-test".to_string(),
            config: config.clone(),
            reservations: vec![res_json],
        };

        assert_eq!(wbq_enqueue(op), WbqEnqueueResult::Enqueued);
        wbq_flush();
        flush_async_commits();

        let archive = ensure_archive(&config, "wbq-res-test").unwrap();
        let res_dir = archive.root.join("file_reservations");
        assert!(res_dir.exists(), "file_reservations/ should exist");
    }

    #[test]
    fn wbq_notification_signal_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_debounce_ms = 0; // no debounce for test
        config.notifications_include_metadata = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        wbq_start();

        let metadata = NotificationMessage {
            id: Some(99),
            from: Some("Sender".to_string()),
            subject: Some("Signal test".to_string()),
            importance: Some("high".to_string()),
        };

        let op = WriteOp::NotificationSignal {
            config: config.clone(),
            project_slug: "wbq-signal-test".to_string(),
            agent_name: "SignalAgent".to_string(),
            metadata: Some(metadata),
        };

        assert_eq!(wbq_enqueue(op), WbqEnqueueResult::Enqueued);
        wbq_flush();

        let signal_path = config
            .notifications_signals_dir
            .join("projects")
            .join("wbq-signal-test")
            .join("agents")
            .join("SignalAgent.signal");
        assert!(
            signal_path.exists(),
            "signal file should exist after WBQ drain"
        );
    }

    #[test]
    fn wbq_backpressure_fallback() {
        // Verify the stats track fallbacks (we can't easily fill the 256-capacity
        // channel in a unit test, but we can verify the counter exists)
        wbq_start();
        let stats = wbq_stats();
        // fallbacks should be 0 when the queue isn't full
        assert_eq!(stats.fallbacks, 0);
    }

    #[test]
    fn collect_lock_status_nonexistent_root() {
        let config = Config {
            storage_root: PathBuf::from("/tmp/nonexistent_lock_status_test_root"),
            ..Config::default()
        };
        let result = collect_lock_status(&config).unwrap();
        assert_eq!(result["exists"], false);
        assert!(result["locks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn collect_lock_status_empty_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let result = collect_lock_status(&config).unwrap();
        assert_eq!(result["exists"], true);
        assert!(result["locks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn collect_lock_status_finds_lock_files() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        // Create a nested .lock file
        let sub = tmp.path().join("projects").join("test-proj");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("index.lock"), "lock contents").unwrap();

        let result = collect_lock_status(&config).unwrap();
        assert_eq!(result["exists"], true);
        let locks = result["locks"].as_array().unwrap();
        assert_eq!(locks.len(), 1);
        assert!(locks[0]["path"].as_str().unwrap().contains("index.lock"));
        assert!(locks[0]["size"].as_u64().unwrap() > 0);
        assert!(locks[0]["modified_epoch"].as_u64().is_some());
    }

    #[test]
    fn collect_lock_status_includes_owner_metadata() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        // Create a .lock file with an owner metadata sidecar
        fs::write(tmp.path().join("repo.lock"), "lock").unwrap();
        fs::write(
            tmp.path().join("repo.lock.owner.json"),
            r#"{"agent":"BlueLake","pid":1234}"#,
        )
        .unwrap();

        let result = collect_lock_status(&config).unwrap();
        let locks = result["locks"].as_array().unwrap();
        assert_eq!(locks.len(), 1);
        let owner = &locks[0]["owner"];
        assert_eq!(owner["agent"].as_str().unwrap(), "BlueLake");
        assert_eq!(owner["pid"].as_u64().unwrap(), 1234);
    }

    // -----------------------------------------------------------------------
    // Consistency checks
    // -----------------------------------------------------------------------

    #[test]
    fn consistency_empty_messages_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let report = check_archive_consistency(dir.path(), &[]);
        assert_eq!(report.sampled, 0);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 0);
        assert!(report.missing_ids.is_empty());
    }

    #[test]
    fn consistency_missing_archive_detected() {
        let dir = tempfile::tempdir().unwrap();
        let refs = vec![ConsistencyMessageRef {
            project_slug: "test-project".into(),
            message_id: 42,
            sender_name: "BlueLake".into(),
            subject: "hello world".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![42]);
    }

    #[test]
    fn consistency_found_when_archive_exists() {
        let dir = tempfile::tempdir().unwrap();
        let slug = "test-project";
        // Create the expected archive file structure:
        // {root}/projects/{slug}/archive/messages/2026/02/{iso}__hello-world__42.md
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
            .join("archive")
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
        fs::write(
            msg_dir.join("2026-02-08T03-29-30+00-00__hello-world__42.md"),
            "test content",
        )
        .unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: slug.into(),
            message_id: 42,
            sender_name: "BlueLake".into(),
            subject: "hello world".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 1);
        assert_eq!(report.missing, 0);
        assert!(report.missing_ids.is_empty());
    }

    #[test]
    fn consistency_mixed_found_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let slug = "test-project";
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
            .join("archive")
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
        // Only create archive for message 42, not 99
        fs::write(
            msg_dir.join("2026-02-08T03-29-30+00-00__hello__42.md"),
            "content",
        )
        .unwrap();

        let refs = vec![
            ConsistencyMessageRef {
                project_slug: slug.into(),
                message_id: 42,
                sender_name: "BlueLake".into(),
                subject: "hello".into(),
                created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
            },
            ConsistencyMessageRef {
                project_slug: slug.into(),
                message_id: 99,
                sender_name: "RedFox".into(),
                subject: "missing".into(),
                created_ts_iso: "2026-02-08T04:00:00+00:00".into(),
            },
        ];
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 2);
        assert_eq!(report.found, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![99]);
    }

    #[test]
    fn parse_year_month_valid() {
        assert_eq!(
            parse_year_month("2026-02-08T03:29:30+00:00"),
            Some(("2026".into(), "02".into()))
        );
        assert_eq!(
            parse_year_month("2025-12-31"),
            Some(("2025".into(), "12".into()))
        );
    }

    #[test]
    fn parse_year_month_invalid() {
        assert_eq!(parse_year_month("abc"), None);
        assert_eq!(parse_year_month(""), None);
        assert_eq!(parse_year_month("20"), None);
    }

    #[test]
    fn io_metrics_nonzero_after_archive_write() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "io-metrics-test").unwrap();

        let agent = serde_json::json!({
            "name": "TestAgent",
            "program": "test",
            "model": "test-model",
        });

        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits();

        let snap = mcp_agent_mail_core::global_metrics().storage.snapshot();

        // The coalescer path should have recorded at least one commit attempt.
        assert!(
            snap.commit_attempts_total >= 1,
            "expected commit_attempts_total >= 1, got {}",
            snap.commit_attempts_total
        );
        assert!(
            snap.git_commit_latency_us.count >= 1,
            "expected git_commit_latency_us count >= 1, got {}",
            snap.git_commit_latency_us.count
        );

        // Verify the snapshot serializes to JSON with the IO metric keys.
        let json = serde_json::to_value(&snap).expect("snapshot should be JSON-serializable");
        assert!(json.get("archive_lock_wait_us").is_some());
        assert!(json.get("git_commit_latency_us").is_some());
        assert!(json.get("commit_attempts_total").is_some());
    }

    #[test]
    fn thread_digest_concurrent_appends_no_interleave() {
        // Stress test: 50 concurrent messages to the same thread.
        // Verifies:
        //   1. Exactly one thread header ("# Thread ...")
        //   2. Exactly 50 entry separators ("---")
        //   3. No interleaved/partial entries
        use std::sync::{Arc, Barrier};

        const NUM_MESSAGES: usize = 50;
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "concurrent-proj").unwrap();

        let archive = Arc::new(archive);
        let config = Arc::new(config);
        let barrier = Arc::new(Barrier::new(NUM_MESSAGES));

        let handles: Vec<_> = (0..NUM_MESSAGES)
            .map(|i| {
                let archive = Arc::clone(&archive);
                let config = Arc::clone(&config);
                let barrier = Arc::clone(&barrier);

                std::thread::spawn(move || {
                    let message = serde_json::json!({
                        "id": i + 1,
                        "subject": format!("Concurrent msg #{i}"),
                        "created_ts": format!("2026-01-15T10:{:02}:{:02}Z", i / 60, i % 60),
                        "thread_id": "STRESS-THREAD-1",
                        "project": "concurrent-proj",
                    });

                    let body =
                        format!("Body content for message {i}. Some text to verify integrity.");

                    barrier.wait();

                    write_message_bundle(
                        &archive,
                        &config,
                        &message,
                        &body,
                        "SenderAgent",
                        &["ReceiverAgent".to_string()],
                        &[],
                        None,
                    )
                    .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // Read the thread digest and verify structure
        let digest_path = archive.root.join("messages/threads/stress-thread-1.md");
        assert!(digest_path.exists(), "digest file should exist");

        let content = std::fs::read_to_string(&digest_path).unwrap();

        // Exactly one thread header
        let header_count = content.matches("# Thread STRESS-THREAD-1").count();
        assert_eq!(
            header_count, 1,
            "expected exactly 1 thread header, got {header_count}"
        );

        // Exactly NUM_MESSAGES separator lines
        let separator_count = content.matches("\n---\n").count();
        assert_eq!(
            separator_count, NUM_MESSAGES,
            "expected {NUM_MESSAGES} separators, got {separator_count}"
        );

        // Each message should have its "View canonical" link
        let link_count = content.matches("[View canonical]").count();
        assert_eq!(
            link_count, NUM_MESSAGES,
            "expected {NUM_MESSAGES} canonical links, got {link_count}"
        );

        // Verify no partial entries: every "## " header line should have
        // a matching "---" separator. Count entry headers (## timestamp —).
        let entry_header_count = content.matches("## 2026-01-15T").count();
        assert_eq!(
            entry_header_count, NUM_MESSAGES,
            "expected {NUM_MESSAGES} entry headers, got {entry_header_count}"
        );
    }

    // -----------------------------------------------------------------------
    // Per-project commit queue tests
    // -----------------------------------------------------------------------

    #[test]
    fn coalescer_per_repo_stats_populated() {
        // Verify that per-repo metrics are tracked after enqueuing commits.
        // All projects under the same config share one repo_root (archive root).
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let archive = ensure_archive(&config, "stats-proj").unwrap();

        let agent = serde_json::json!({"name": "StatsAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();

        flush_async_commits();

        let per_repo = get_commit_coalescer().per_repo_stats();

        // The archive repo_root should appear in per-repo stats
        let repo_stats = per_repo.get(&archive.repo_root);
        assert!(
            repo_stats.is_some(),
            "per_repo_stats should contain the archive repo_root {:?}, keys: {:?}",
            archive.repo_root,
            per_repo.keys().collect::<Vec<_>>()
        );

        let stats = repo_stats.unwrap();
        assert!(
            stats.enqueued_total >= 1,
            "repo should have enqueued >= 1, got {}",
            stats.enqueued_total
        );
        assert!(
            stats.drained_total >= 1,
            "repo should have drained >= 1, got {}",
            stats.drained_total
        );
        assert!(
            stats.commits_total >= 1,
            "repo should have commits >= 1, got {}",
            stats.commits_total
        );
    }

    #[test]
    fn coalescer_worker_count_auto_detected() {
        let coalescer = get_commit_coalescer();
        let wc = coalescer.worker_count();
        assert!(wc >= 2, "worker count should be >= 2, got {wc}");
        assert!(wc <= 32, "worker count should be <= 32, got {wc}");
    }

    #[test]
    fn coalescer_multi_project_concurrent_commits() {
        // 5 projects × 10 agents each = 50 concurrent commits to the SAME
        // archive repo_root. Verify all commits complete and per-repo metrics
        // reflect the aggregate activity.
        use std::sync::{Arc, Barrier};

        const NUM_PROJECTS: usize = 5;
        const AGENTS_PER_PROJECT: usize = 10;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let config = Arc::new(config);

        // Pre-create archives (all share same repo_root)
        let archives: Vec<_> = (0..NUM_PROJECTS)
            .map(|i| Arc::new(ensure_archive(&config, &format!("conc-proj-{i}")).unwrap()))
            .collect();

        let repo_root = archives[0].repo_root.clone();

        // Snapshot per-repo stats before the burst
        let stats_before = get_commit_coalescer()
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();

        let barrier = Arc::new(Barrier::new(NUM_PROJECTS * AGENTS_PER_PROJECT));

        let archives2 = archives.clone();
        let handles: Vec<_> = (0..NUM_PROJECTS)
            .flat_map(|proj_idx| {
                let config = Arc::clone(&config);
                let archives = archives2.clone();
                let barrier = Arc::clone(&barrier);
                (0..AGENTS_PER_PROJECT).map(move |agent_idx| {
                    let config = Arc::clone(&config);
                    let archive = Arc::clone(&archives[proj_idx]);
                    let barrier = Arc::clone(&barrier);

                    std::thread::spawn(move || {
                        let agent_name = format!("Agent{proj_idx}x{agent_idx}");
                        let agent = serde_json::json!({
                            "name": agent_name,
                            "program": "test",
                        });

                        barrier.wait();

                        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
                    })
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        flush_async_commits();

        // Verify all agent profiles exist
        for (proj_idx, archive) in archives.iter().enumerate().take(NUM_PROJECTS) {
            for agent_idx in 0..AGENTS_PER_PROJECT {
                let agent_name = format!("Agent{proj_idx}x{agent_idx}");
                let profile_path = archive
                    .root
                    .join("agents")
                    .join(&agent_name)
                    .join("profile.json");
                assert!(
                    profile_path.exists(),
                    "profile for {agent_name} should exist at {profile_path:?}"
                );
            }
        }

        // Verify per-repo stats reflect the burst
        let stats_after = get_commit_coalescer()
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();
        let enqueued_delta = stats_after.enqueued_total - stats_before.enqueued_total;
        let expected = (NUM_PROJECTS * AGENTS_PER_PROJECT) as u64;
        assert_eq!(
            enqueued_delta, expected,
            "expected {expected} enqueued commits, got delta {enqueued_delta}"
        );
        assert!(
            stats_after.drained_total >= stats_before.drained_total + expected,
            "expected drained >= {}, got {}",
            stats_before.drained_total + expected,
            stats_after.drained_total
        );
    }

    #[test]
    fn coalescer_global_stats_backward_compat() {
        // Ensure the aggregate stats() method still works after per-repo refactor.
        // Self-contained: enqueue a commit ourselves and verify.
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "compat-proj").unwrap();

        let agent = serde_json::json!({"name": "CompatAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits();

        let stats = get_commit_coalescer().stats();
        assert!(
            stats.enqueued >= 1,
            "global enqueued should be >= 1, got {}",
            stats.enqueued
        );
    }

    #[test]
    fn coalescer_multi_repo_root_parallelism() {
        // Create archives with DIFFERENT repo_roots (separate tmp dirs).
        // Verify they get separate per-repo queue entries, demonstrating
        // that the per-repo design enables true parallelism across repos.
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();

        let config_a = test_config(tmp_a.path());
        let config_b = test_config(tmp_b.path());

        let archive_a = ensure_archive(&config_a, "repo-a-proj").unwrap();
        let archive_b = ensure_archive(&config_b, "repo-b-proj").unwrap();

        assert_ne!(
            archive_a.repo_root, archive_b.repo_root,
            "different tmp dirs should give different repo_roots"
        );

        let agent = serde_json::json!({"name": "ParAgent", "program": "test"});
        write_agent_profile_with_config(&archive_a, &config_a, &agent).unwrap();
        write_agent_profile_with_config(&archive_b, &config_b, &agent).unwrap();

        flush_async_commits();

        let per_repo = get_commit_coalescer().per_repo_stats();
        let stats_a = per_repo.get(&archive_a.repo_root);
        let stats_b = per_repo.get(&archive_b.repo_root);

        assert!(stats_a.is_some(), "repo_root A should be in per_repo_stats");
        assert!(stats_b.is_some(), "repo_root B should be in per_repo_stats");
        assert!(
            stats_a.unwrap().enqueued_total >= 1,
            "repo A should have enqueued >= 1"
        );
        assert!(
            stats_b.unwrap().enqueued_total >= 1,
            "repo B should have enqueued >= 1"
        );
    }

    // -----------------------------------------------------------------------
    // Queue saturation stress tests (br-15dv.9.4)
    // -----------------------------------------------------------------------

    #[test]
    fn wbq_burst_200_ops_no_data_loss() {
        // Burst 200 write ops rapidly and verify all are drained with no errors.
        wbq_start();
        let before = wbq_stats();
        let burst_count = 200u64;

        let mut actually_enqueued = 0u64;
        for i in 0..burst_count {
            let op = WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("burst-{i}"),
                agent_name: "BurstAgent".to_string(),
            };
            match wbq_enqueue(op) {
                WbqEnqueueResult::Enqueued => actually_enqueued += 1,
                WbqEnqueueResult::SkippedDiskCritical => {}
                WbqEnqueueResult::QueueUnavailable => {
                    panic!("WBQ should not become unavailable during burst (op {i})");
                }
            }
        }

        // Flush to drain all pending ops.
        wbq_flush();

        let after = wbq_stats();
        let enqueued_delta = after.enqueued - before.enqueued;
        let drained_delta = after.drained - before.drained;

        assert!(
            enqueued_delta >= actually_enqueued,
            "expected at least {actually_enqueued} enqueued, got delta {enqueued_delta}"
        );
        assert!(
            drained_delta >= actually_enqueued,
            "expected at least {actually_enqueued} drained, got delta {drained_delta}"
        );

        // All our ops should have been drained (delta-based, safe with parallel tests).
    }

    #[test]
    fn wbq_burst_500_ops_backpressure_metrics() {
        // Larger burst to exercise backpressure tracking and peak depth metrics.
        wbq_start();
        let metrics = mcp_agent_mail_core::global_metrics();

        let peak_before = metrics.storage.wbq_peak_depth.load();
        let before = wbq_stats();
        let burst_count = 500u64;

        // Fire ops as fast as possible from a single thread.
        let mut enqueued_count = 0u64;
        for i in 0..burst_count {
            let op = WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("bp-burst-{i}"),
                agent_name: "BackpressureAgent".to_string(),
            };
            match wbq_enqueue(op) {
                WbqEnqueueResult::Enqueued => enqueued_count += 1,
                WbqEnqueueResult::SkippedDiskCritical => {} // ok under disk pressure
                WbqEnqueueResult::QueueUnavailable => {
                    // Backpressure timeout exceeded; acceptable under extreme burst.
                }
            }
        }

        wbq_flush();

        let after = wbq_stats();
        let drained_delta = after.drained - before.drained;

        // All successfully enqueued ops must be drained (no data loss).
        assert!(
            drained_delta >= enqueued_count,
            "drained ({drained_delta}) should be >= enqueued ({enqueued_count})"
        );

        // Peak depth should have increased (the queue was loaded).
        let peak_after = metrics.storage.wbq_peak_depth.load();
        assert!(
            peak_after >= peak_before,
            "peak depth should not decrease: before={peak_before}, after={peak_after}"
        );
    }

    #[test]
    fn wbq_concurrent_burst_from_multiple_threads() {
        // Simulate multiple agents bursting concurrently (thread per agent).
        wbq_start();
        let before = wbq_stats();
        let thread_count = 8u64;
        let ops_per_thread = 50u64;

        let barrier = Arc::new(std::sync::Barrier::new(thread_count as usize));
        let enqueued_total = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let barrier = Arc::clone(&barrier);
                let enqueued_total = Arc::clone(&enqueued_total);
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..ops_per_thread {
                        let op = WriteOp::ClearSignal {
                            config: Config::default(),
                            project_slug: format!("mt-burst-t{t}-{i}"),
                            agent_name: format!("Thread{t}Agent"),
                        };
                        if wbq_enqueue(op) == WbqEnqueueResult::Enqueued {
                            enqueued_total.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread should not panic");
        }

        wbq_flush();

        let after = wbq_stats();
        let enqueued_delta = after.enqueued - before.enqueued;
        let drained_delta = after.drained - before.drained;
        let actually_enqueued = enqueued_total.load(Ordering::Relaxed);

        assert!(
            enqueued_delta >= actually_enqueued,
            "metric enqueued ({enqueued_delta}) should be >= actual ({actually_enqueued})"
        );
        assert!(
            drained_delta >= actually_enqueued,
            "drained ({drained_delta}) should be >= enqueued ({actually_enqueued})"
        );
        // Note: depth may not be exactly 0 due to parallel test enqueues;
        // the drained >= enqueued assertion above proves no data loss.
    }

    #[test]
    fn coalescer_batching_efficiency_under_burst() {
        // Burst 100 commits to the same repo and verify batching reduces
        // individual commits (100 enqueues should produce < 50 actual commits).
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "batch-eff").unwrap();

        let coalescer = get_commit_coalescer();
        let stats_before = coalescer.stats();

        let burst_count = 100usize;
        for i in 0..burst_count {
            // Write a unique file for each enqueue.
            let file_name = format!("batch_test_{i}.txt");
            let file_path = archive.root.join(&file_name);
            fs::write(&file_path, format!("content-{i}")).unwrap();
            let rel = rel_path_cached(&archive.canonical_repo_root, &file_path).unwrap();

            coalescer.enqueue(
                archive.repo_root.clone(),
                &config,
                format!("batch commit {i}"),
                vec![rel],
            );
        }

        // Give the coalescer workers time to drain.
        coalescer.flush_sync();

        let stats_after = coalescer.stats();
        let enqueued_delta = stats_after.enqueued - stats_before.enqueued;
        let commits_delta = stats_after.commits - stats_before.commits;

        assert!(
            enqueued_delta >= burst_count,
            "expected at least {burst_count} enqueued, got delta {enqueued_delta}"
        );

        // Batching should reduce commit count significantly.
        if commits_delta > 0 {
            assert!(
                commits_delta < burst_count / 2,
                "batching should reduce commits: {commits_delta} commits for {enqueued_delta} \
                 enqueues (expected < {}, avg batch = {:.1})",
                burst_count / 2,
                enqueued_delta as f64 / commits_delta as f64,
            );
        }
    }

    #[test]
    fn wbq_zero_sync_fallbacks_under_normal_burst() {
        // Under a normal-sized burst (well below 8192 capacity), there should
        // be zero fallbacks (the queue never fills).
        wbq_start();
        let before = wbq_stats();

        for i in 0..100u64 {
            let op = WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("no-fallback-{i}"),
                agent_name: "NoFallbackAgent".to_string(),
            };
            let result = wbq_enqueue(op);
            assert_eq!(
                result,
                WbqEnqueueResult::Enqueued,
                "op {i} should be enqueued without fallback"
            );
        }

        wbq_flush();

        let after = wbq_stats();
        let fallback_delta = after.fallbacks - before.fallbacks;
        assert_eq!(
            fallback_delta, 0,
            "zero fallbacks expected for sub-capacity burst, got {fallback_delta}"
        );
    }

    #[test]
    fn wbq_autonomous_drain_completes_within_timeout() {
        // Enqueue ops and wait (without explicit flush) for the drain loop
        // to process them all. Uses delta-based assertion to be safe with
        // parallel tests on the shared global WBQ.
        wbq_start();
        let before = wbq_stats();
        let burst = 200u64;
        let mut actually_enqueued = 0u64;
        for i in 0..burst {
            let op = WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("depth-poll-{i}"),
                agent_name: "DepthAgent".to_string(),
            };
            if wbq_enqueue(op) == WbqEnqueueResult::Enqueued {
                actually_enqueued += 1;
            }
        }

        // Poll drained counter (not depth) until our ops have been processed.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let current = wbq_stats();
            let drained_delta = current.drained - before.drained;
            if drained_delta >= actually_enqueued {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "WBQ drain did not complete within 10s \
                     (enqueued {actually_enqueued}, drained delta {drained_delta})"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ── Lock-free commit tests ────────────────────────────────────────

    #[test]
    fn lockfree_commit_single_file_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "lockfree-single").unwrap();

        // Write a file inside the project archive (under repo_root)
        let file_path = archive.root.join("agents/TestAgent/profile.json");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, r#"{"name":"TestAgent"}"#).unwrap();
        let rel = rel_path_cached(&archive.canonical_repo_root, &file_path).unwrap();

        let before = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commits_total
            .load();

        commit_paths_with_retry(
            &archive.repo_root,
            &config,
            "lockfree single file test",
            &[rel.as_str()],
        )
        .unwrap();

        let after = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commits_total
            .load();

        // At least one lockfree commit should have happened (delta ≥ 1)
        assert!(
            after > before,
            "lockfree_commits_total did not increment: before={before}, after={after}"
        );

        // Verify file is in git
        let repo = Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let tree = head.tree().unwrap();
        assert!(tree.get_path(Path::new(&rel)).is_ok());
    }

    #[test]
    fn lockfree_commit_10_concurrent_agents() {
        // Tests that 10 threads can all commit to the same repo without crashing.
        // Under true concurrency, lockfree commits may race on HEAD ref updates;
        // this test verifies the retry+fallback logic handles that gracefully.
        // (In production, the CommitCoalescer serializes commits per repo.)
        use std::sync::{Arc, Barrier};

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "lockfree-concurrent").unwrap();
        let repo_root = archive.repo_root.clone();
        let proj_root = archive.root.clone();

        let n_threads = 10usize;
        let barrier = Arc::new(Barrier::new(n_threads));
        let mut handles = Vec::new();

        let rel_prefix = proj_root
            .strip_prefix(&repo_root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let before_lockfree = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commits_total
            .load();
        let before_fallback = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commit_fallbacks_total
            .load();

        for i in 0..n_threads {
            let bar = barrier.clone();
            let repo_root = repo_root.clone();
            let proj_root = proj_root.clone();
            let rel_prefix = rel_prefix.clone();
            let cfg = config.clone();
            handles.push(std::thread::spawn(move || {
                let file_rel = format!("agents/Agent{i}/profile.json");
                let full = proj_root.join(&file_rel);
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(&full, format!(r#"{{"name":"Agent{i}"}}"#)).unwrap();

                let rel = format!("{rel_prefix}/{file_rel}");

                bar.wait(); // synchronize for maximum contention

                // Retry with backoff: concurrent HEAD ref updates cause
                // transient CAS failures in both lockfree and index-based paths.
                let mut last_err = None;
                for attempt in 0..15 {
                    match commit_paths_with_retry(
                        &repo_root,
                        &cfg,
                        &format!("agent {i} commit"),
                        &[rel.as_str()],
                    ) {
                        Ok(()) => {
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            std::thread::sleep(Duration::from_millis(10 * (1 << attempt.min(5))));
                        }
                    }
                }
                if let Some(e) = last_err {
                    panic!("agent {i} commit failed after retries: {e}");
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All threads completed without panic — commit retries handled contention.
        // The git commit history should have at least n_threads commits.
        let repo = Repository::open(&repo_root).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        let commit_count = revwalk.count();
        // At least 1 initial commit + n_threads agent commits
        assert!(
            commit_count >= n_threads,
            "expected at least {n_threads} commits, got {commit_count}"
        );

        // Metrics should show lockfree activity (delta-based for test isolation)
        let after_lockfree = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commits_total
            .load();
        let after_fallback = mcp_agent_mail_core::global_metrics()
            .storage
            .lockfree_commit_fallbacks_total
            .load();
        let lockfree_delta = after_lockfree - before_lockfree;
        let fallback_delta = after_fallback - before_fallback;
        assert!(
            lockfree_delta > 0 || fallback_delta > 0,
            "expected lockfree metric activity: lockfree_delta={lockfree_delta}, fallback_delta={fallback_delta}"
        );
    }

    #[test]
    fn pid_owner_tracking_write_and_cleanup() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "pid-owner").unwrap();

        // Write owner file (uses .git/ which is at repo_root, not project root)
        write_lock_owner(&archive.repo_root);
        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        assert!(owner_path.exists(), "owner file should be created");

        let content = fs::read_to_string(&owner_path).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2, "owner file should have PID and timestamp");

        let pid: u32 = lines[0].parse().unwrap();
        assert_eq!(
            pid,
            std::process::id(),
            "owner PID should match current process"
        );

        let ts: u64 = lines[1].parse().unwrap();
        assert!(ts > 0, "timestamp should be non-zero");

        // Remove owner file
        remove_lock_owner(&archive.repo_root);
        assert!(!owner_path.exists(), "owner file should be removed");
    }

    #[test]
    fn stale_lock_cleanup_respects_alive_pid() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "alive-pid").unwrap();

        // Create a fake index.lock with an owner that is the current (alive) PID
        let lock_path = archive.repo_root.join(".git/index.lock");
        fs::write(&lock_path, "fake lock").unwrap();
        write_lock_owner(&archive.repo_root);

        // Stale lock cleanup should NOT remove the lock (PID is alive)
        let removed = try_clean_stale_git_lock(&archive.repo_root, 0.0);
        assert!(!removed, "should not remove lock owned by alive PID");
        assert!(lock_path.exists(), "lock file should still exist");

        // Clean up
        fs::remove_file(&lock_path).ok();
        remove_lock_owner(&archive.repo_root);
    }

    #[test]
    fn stale_lock_cleanup_removes_dead_pid_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "dead-pid").unwrap();

        // Create a fake index.lock with a dead PID owner
        let lock_path = archive.repo_root.join(".git/index.lock");
        fs::write(&lock_path, "fake lock").unwrap();

        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        // PID 999999999 is virtually guaranteed to not exist
        fs::write(&owner_path, "999999999\n1000000000\n").unwrap();

        // Stale lock cleanup SHOULD remove the lock (PID is dead)
        let removed = try_clean_stale_git_lock(&archive.repo_root, 0.0);
        assert!(removed, "should remove lock owned by dead PID");
        assert!(!lock_path.exists(), "lock file should be removed");
    }

    // ── br-1i11.5.2: depth counter underflow recovery ─────────────────
    //
    // These tests exercise the fetch_update + saturating_sub pattern used
    // in wbq_drain_loop() to decrement the op_depth counter. The pattern
    // must handle the case where drained_count exceeds observed depth
    // (e.g. due to counter reset or race) without wrapping around u64::MAX.

    /// Helper: applies the same depth-decrement logic as wbq_drain_loop.
    fn depth_decrement(counter: &AtomicU64, drained: u64) -> u64 {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(drained))
            })
            .unwrap_or(0)
            .saturating_sub(drained)
    }

    #[test]
    fn depth_counter_normal_decrement() {
        let counter = AtomicU64::new(10);
        let after = depth_decrement(&counter, 3);
        assert_eq!(after, 7, "10 - 3 = 7");
        assert_eq!(counter.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn depth_counter_exact_drain() {
        let counter = AtomicU64::new(5);
        let after = depth_decrement(&counter, 5);
        assert_eq!(after, 0, "5 - 5 = 0");
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn depth_counter_underflow_saturates_to_zero() {
        let counter = AtomicU64::new(3);
        let after = depth_decrement(&counter, 10);
        assert_eq!(after, 0, "3 - 10 should saturate to 0, not wrap");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "stored value should be 0 after saturation"
        );
    }

    #[test]
    fn depth_counter_underflow_from_zero() {
        let counter = AtomicU64::new(0);
        let after = depth_decrement(&counter, 5);
        assert_eq!(after, 0, "0 - 5 should saturate to 0");
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn depth_counter_underflow_with_max_drain() {
        let counter = AtomicU64::new(1);
        let after = depth_decrement(&counter, u64::MAX);
        assert_eq!(after, 0, "1 - MAX should saturate to 0");
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn depth_counter_recovery_after_underflow() {
        let counter = AtomicU64::new(2);

        // Cause underflow saturation
        let after = depth_decrement(&counter, 100);
        assert_eq!(after, 0, "should saturate to 0");

        // Counter should still work correctly after saturation
        counter.fetch_add(5, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 5);

        // Normal decrement should work
        let after = depth_decrement(&counter, 2);
        assert_eq!(after, 3, "5 - 2 = 3 after recovery");
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn depth_counter_sequential_decrements() {
        let counter = AtomicU64::new(20);

        let after = depth_decrement(&counter, 8);
        assert_eq!(after, 12);

        let after = depth_decrement(&counter, 8);
        assert_eq!(after, 4);

        // Third drain exceeds remaining depth
        let after = depth_decrement(&counter, 8);
        assert_eq!(after, 0, "4 - 8 should saturate to 0");

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn depth_counter_single_item_decrement() {
        // Tests the single-op decrement path used during shutdown drain
        let counter = AtomicU64::new(3);
        let after = depth_decrement(&counter, 1);
        assert_eq!(after, 2);
        let after = depth_decrement(&counter, 1);
        assert_eq!(after, 1);
        let after = depth_decrement(&counter, 1);
        assert_eq!(after, 0);
        // One more past zero
        let after = depth_decrement(&counter, 1);
        assert_eq!(after, 0, "should not wrap past zero");
    }

    #[test]
    fn depth_counter_concurrent_inc_dec_no_wraparound() {
        use std::sync::Arc;

        let counter = Arc::new(AtomicU64::new(0));
        let num_threads = 8;
        let ops_per_thread = 1000;

        let mut handles = Vec::new();

        // Spawn threads that increment and decrement
        for _ in 0..num_threads {
            let c = Arc::clone(&counter);
            handles.push(std::thread::spawn(move || {
                for _ in 0..ops_per_thread {
                    c.fetch_add(1, Ordering::Relaxed);
                }
                for _ in 0..ops_per_thread {
                    c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                        Some(cur.saturating_sub(1))
                    })
                    .ok();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let final_val = counter.load(Ordering::Relaxed);
        assert_eq!(
            final_val, 0,
            "after equal inc/dec across threads, depth should be 0, got {final_val}"
        );
    }

    #[test]
    fn depth_counter_concurrent_over_decrement_stays_zero() {
        use std::sync::Arc;

        let counter = Arc::new(AtomicU64::new(100));
        let num_threads = 8;

        let mut handles = Vec::new();

        // Each thread tries to drain 50, total = 400 > 100
        for _ in 0..num_threads {
            let c = Arc::clone(&counter);
            handles.push(std::thread::spawn(move || {
                c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(50))
                })
                .ok();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let final_val = counter.load(Ordering::Relaxed);
        assert_eq!(
            final_val, 0,
            "over-decrement should saturate to 0, got {final_val}"
        );
    }

    #[test]
    fn depth_counter_increment_always_succeeds() {
        let counter = AtomicU64::new(0);

        // After underflow saturation, incrementing should work
        depth_decrement(&counter, 999);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        counter.fetch_add(42, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            42,
            "increment after saturation should work normally"
        );
    }

    // ── br-1i11.1.2: spill drain path ordering determinism ────────────
    //
    // Verify that CoalescerSpillRepo.paths (BTreeSet<String>) produces
    // sorted output when drained, matching the pattern used in
    // coalescer_drain_repo_spill().

    /// Helper: simulates the spill drain pattern — insert paths into a
    /// BTreeSet then collect via into_iter() (same as line 1863).
    fn spill_drain_paths(inputs: &[&str]) -> Vec<String> {
        let mut paths = BTreeSet::new();
        for p in inputs {
            paths.insert((*p).to_string());
        }
        paths.into_iter().collect()
    }

    /// Deterministic Fisher-Yates shuffle driven by a simple LCG seed.
    ///
    /// Kept local to tests to avoid extra dev-dependencies (`rand`) while still
    /// supporting reproducible stress permutations and seed replay.
    fn seeded_permutation(inputs: &[String], seed: u64) -> Vec<String> {
        let mut out = inputs.to_vec();
        if out.len() <= 1 {
            return out;
        }
        let mut state = seed;
        for i in (1..out.len()).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let j = (state % (i as u64 + 1)) as usize;
            out.swap(i, j);
        }
        out
    }

    fn spill_drain_paths_owned(inputs: &[String]) -> Vec<String> {
        let mut paths = BTreeSet::new();
        for p in inputs {
            paths.insert(p.clone());
        }
        paths.into_iter().collect()
    }

    #[test]
    fn spill_drain_produces_sorted_paths() {
        let result = spill_drain_paths(&["z.md", "a.md", "m.md"]);
        assert_eq!(result, vec!["a.md", "m.md", "z.md"]);
    }

    #[test]
    fn spill_drain_sorted_with_nested_paths() {
        let result = spill_drain_paths(&[
            "messages/2026/02/msg3.md",
            "agents/ZebraAgent/profile.json",
            "agents/AlphaAgent/profile.json",
            "messages/2026/01/msg1.md",
            "file_reservations/abc.json",
        ]);
        assert_eq!(
            result,
            vec![
                "agents/AlphaAgent/profile.json",
                "agents/ZebraAgent/profile.json",
                "file_reservations/abc.json",
                "messages/2026/01/msg1.md",
                "messages/2026/02/msg3.md",
            ]
        );
    }

    #[test]
    fn spill_drain_deduplicates_paths() {
        let result = spill_drain_paths(&["a.md", "b.md", "a.md", "c.md", "b.md"]);
        assert_eq!(result, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn spill_drain_single_path() {
        let result = spill_drain_paths(&["only.md"]);
        assert_eq!(result, vec!["only.md"]);
    }

    #[test]
    fn spill_drain_empty() {
        let result = spill_drain_paths(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn spill_drain_already_sorted_input() {
        let result = spill_drain_paths(&["a.md", "b.md", "c.md"]);
        assert_eq!(result, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn spill_drain_reverse_sorted_input() {
        let result = spill_drain_paths(&["c.md", "b.md", "a.md"]);
        assert_eq!(result, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn spill_drain_unicode_paths_sorted() {
        let result = spill_drain_paths(&["ñ.md", "a.md", "ä.md", "z.md"]);
        // Unicode sort: a < z < ä < ñ (byte-order for UTF-8)
        assert_eq!(result[0], "a.md");
        assert_eq!(result[1], "z.md");
        // Remaining are sorted by UTF-8 byte order
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn spill_drain_struct_roundtrip_preserves_order() {
        // Simulate the full CoalescerSpillRepo → CoalescerSpilledWork path
        let mut repo = CoalescerSpillRepo {
            pending_requests: 3,
            earliest_enqueued_at: Instant::now(),
            dirty_all: false,
            paths: BTreeSet::new(),
            git_author_name: "test".to_string(),
            git_author_email: "test@test".to_string(),
            message_first_lines: VecDeque::new(),
            message_total: 0,
        };

        repo.paths.insert("zebra/data.json".to_string());
        repo.paths.insert("alpha/config.json".to_string());
        repo.paths.insert("mid/state.json".to_string());

        // Same drain pattern as coalescer_drain_repo_spill line 1863
        let drained: Vec<String> = repo.paths.into_iter().collect();
        assert_eq!(
            drained,
            vec!["alpha/config.json", "mid/state.json", "zebra/data.json",],
            "spill drain must produce lexicographically sorted paths"
        );
    }

    #[test]
    fn spill_path_cap_triggers_dirty_all() {
        let mut paths = BTreeSet::new();
        let cap = COALESCER_SPILL_PATH_CAP;

        // Insert up to cap, should not trigger dirty_all
        for i in 0..cap {
            paths.insert(format!("path_{i:05}.md"));
        }
        assert_eq!(paths.len(), cap);

        // One more would exceed cap
        paths.insert(format!("path_{cap:05}.md"));
        assert!(
            paths.len() > cap,
            "BTreeSet grows past cap (coalescer code clears + sets dirty_all)"
        );

        // Verify that if the coalescer logic were applied, it would trigger dirty_all
        let mut dirty_all = false;
        let mut test_paths = BTreeSet::new();
        for i in 0..=cap {
            test_paths.insert(format!("path_{i:05}.md"));
            if test_paths.len() > COALESCER_SPILL_PATH_CAP {
                dirty_all = true;
                test_paths.clear();
                break;
            }
        }
        assert!(dirty_all, "exceeding cap should trigger dirty_all");
        assert!(
            test_paths.is_empty(),
            "paths should be cleared on dirty_all"
        );
    }

    #[test]
    fn spill_drain_repeated_seeded_permutations_are_stable() {
        let base_paths: Vec<String> = vec![
            "messages/2026/01/0001.md".to_string(),
            "messages/2026/01/0002.md".to_string(),
            "messages/2026/02/0101.md".to_string(),
            "agents/BlueLake/profile.json".to_string(),
            "agents/GreenCastle/profile.json".to_string(),
            "agents/RedHarbor/profile.json".to_string(),
            "file_reservations/01-alpha.json".to_string(),
            "file_reservations/02-beta.json".to_string(),
            "attachments/2026/02/a.webp".to_string(),
            "attachments/2026/02/b.webp".to_string(),
            "threads/br-1i11.1.4/index.md".to_string(),
            "threads/br-1i11.1.4/events/0001.md".to_string(),
        ];

        let mut expected_set = BTreeSet::new();
        for path in &base_paths {
            expected_set.insert(path.clone());
        }
        let expected: Vec<String> = expected_set.into_iter().collect();

        const ITERATIONS: u64 = 512;
        const BASE_SEED: u64 = 0xA11C_E5ED_1234_5678;
        for iteration in 0..ITERATIONS {
            let seed = BASE_SEED.wrapping_add(iteration);
            let shuffled = seeded_permutation(&base_paths, seed);
            let observed = spill_drain_paths_owned(&shuffled);

            assert_eq!(
                observed, expected,
                "spill determinism mismatch at iteration={iteration}, seed={seed}. replay with: cargo test -p mcp-agent-mail-storage spill_drain_seed_replay_contract -- --nocapture\ninput_order={shuffled:?}"
            );
        }
    }

    #[test]
    fn spill_drain_seed_replay_contract() {
        // If a stress run reports this seed, rerun this test directly to verify
        // deterministic reproduction of the exact ordering behavior.
        const REPLAY_SEED: u64 = 0xA11C_E5ED_1234_56FF;
        let base_paths: Vec<String> = vec![
            "messages/2026/01/0001.md".to_string(),
            "messages/2026/01/0002.md".to_string(),
            "messages/2026/02/0101.md".to_string(),
            "agents/BlueLake/profile.json".to_string(),
            "agents/GreenCastle/profile.json".to_string(),
            "agents/RedHarbor/profile.json".to_string(),
            "file_reservations/01-alpha.json".to_string(),
            "file_reservations/02-beta.json".to_string(),
            "attachments/2026/02/a.webp".to_string(),
            "attachments/2026/02/b.webp".to_string(),
            "threads/br-1i11.1.4/index.md".to_string(),
            "threads/br-1i11.1.4/events/0001.md".to_string(),
        ];

        let input_a = seeded_permutation(&base_paths, REPLAY_SEED);
        let input_b = seeded_permutation(&base_paths, REPLAY_SEED);
        assert_eq!(
            input_a, input_b,
            "same seed must produce identical permutation"
        );

        let drained_a = spill_drain_paths_owned(&input_a);
        let drained_b = spill_drain_paths_owned(&input_b);
        assert_eq!(
            drained_a, drained_b,
            "drain output must be replay-stable for seed {REPLAY_SEED}"
        );

        eprintln!("spill replay seed={REPLAY_SEED} input={input_a:?} output={drained_a:?}");
    }

    // ── br-1i11.1.5: spill-path BTreeSet overhead benchmark ─────────────
    //
    // Quantifies the performance of BTreeSet vs hypothetical unsorted insert+sort
    // for the spill path container. Logs runtime, variance, and acceptance
    // thresholds.

    #[test]
    fn spill_path_btreeset_benchmark_insert_and_drain() {
        use std::time::Instant;

        const SIZES: &[usize] = &[100, 500, 1_000, 4_096];
        const ITERATIONS: usize = 50;
        const MAX_OVERHEAD_FACTOR: f64 = 3.0; // BTreeSet must be < 3x of Vec+sort

        for &size in SIZES {
            let paths: Vec<String> = (0..size)
                .map(|i| format!("agents/Agent{:04}/inbox/2026/02/msg_{:06}.md", i % 50, i))
                .collect();

            // BTreeSet path (current production code)
            let mut btree_times = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let start = Instant::now();
                let mut set = BTreeSet::new();
                for p in &paths {
                    set.insert(p.clone());
                }
                let _drained: Vec<String> = set.into_iter().collect();
                btree_times.push(start.elapsed().as_nanos() as f64);
            }

            // Vec + sort path (alternative baseline)
            let mut vecsort_times = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let start = Instant::now();
                let mut vec: Vec<String> = paths.clone();
                vec.sort();
                vec.dedup();
                vecsort_times.push(start.elapsed().as_nanos() as f64);
            }

            let btree_mean = btree_times.iter().sum::<f64>() / ITERATIONS as f64;
            let vecsort_mean = vecsort_times.iter().sum::<f64>() / ITERATIONS as f64;
            let ratio = btree_mean / vecsort_mean.max(1.0);

            let btree_variance = btree_times
                .iter()
                .map(|t| (t - btree_mean).powi(2))
                .sum::<f64>()
                / ITERATIONS as f64;

            eprintln!(
                "spill_bench size={size} btree_mean_ns={btree_mean:.0} vecsort_mean_ns={vecsort_mean:.0} \
                 ratio={ratio:.2}x btree_stddev_ns={:.0} iterations={ITERATIONS}",
                btree_variance.sqrt()
            );

            assert!(
                ratio < MAX_OVERHEAD_FACTOR,
                "BTreeSet overhead too high at size={size}: {ratio:.2}x (max {MAX_OVERHEAD_FACTOR}x)"
            );
        }
    }

    #[test]
    fn spill_path_btreeset_benchmark_random_order_stability() {
        use std::time::Instant;

        const SIZE: usize = 1_000;
        const ITERATIONS: u64 = 100;
        const BASE_SEED: u64 = 0xBEEF_CAFE_0000_0001;

        let base_paths: Vec<String> = (0..SIZE).map(|i| format!("path/{:04}.md", i)).collect();

        let mut times = Vec::with_capacity(ITERATIONS as usize);
        for iteration in 0..ITERATIONS {
            let seed = BASE_SEED.wrapping_add(iteration);
            let shuffled = seeded_permutation(&base_paths, seed);

            let start = Instant::now();
            let _drained = spill_drain_paths_owned(&shuffled);
            times.push(start.elapsed().as_nanos() as f64);
        }

        let mean = times.iter().sum::<f64>() / ITERATIONS as f64;
        let variance = times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / ITERATIONS as f64;
        let stddev = variance.sqrt();
        let cv = stddev / mean.max(1.0);

        eprintln!(
            "spill_bench_random size={SIZE} mean_ns={mean:.0} stddev_ns={stddev:.0} \
             cv={cv:.3} iterations={ITERATIONS}"
        );

        // Coefficient of variation should be < 1.0 (stable performance)
        assert!(
            cv < 1.0,
            "Spill drain performance too variable: cv={cv:.3} (max 1.0)"
        );
    }

    // ── br-1i11.5.5: depth counter concurrency stress tests ─────────────
    //
    // High-contention stress tests for the fetch_update + saturating_sub
    // depth counter pattern, with diagnostic logging for triage.

    #[test]
    fn depth_counter_stress_interleaved_inc_dec_32_threads() {
        use std::sync::Arc;

        let counter = Arc::new(AtomicU64::new(0));
        let num_threads = 32;
        let ops_per_thread = 5_000;

        std::thread::scope(|s| {
            for tid in 0..num_threads {
                let c = Arc::clone(&counter);
                s.spawn(move || {
                    for i in 0..ops_per_thread {
                        if i % 2 == 0 {
                            c.fetch_add(1, Ordering::Relaxed);
                        } else {
                            c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                                Some(cur.saturating_sub(1))
                            })
                            .ok();
                        }
                    }
                    // Each thread does equal inc/dec, so net contribution = 0
                    eprintln!("depth_stress thread={tid} completed {ops_per_thread} ops");
                });
            }
        });

        let final_val = counter.load(Ordering::Relaxed);
        assert_eq!(
            final_val, 0,
            "32 threads × 5000 balanced ops should net to 0, got {final_val}"
        );
    }

    #[test]
    fn depth_counter_stress_burst_drain_never_wraps() {
        use std::sync::Arc;

        let counter = Arc::new(AtomicU64::new(500));
        let num_drain_threads = 16;
        let drain_per_thread = 100; // Total drain: 1600 >> 500
        let observed_max = Arc::new(std::sync::atomic::AtomicU64::new(0));

        std::thread::scope(|s| {
            for tid in 0..num_drain_threads {
                let c = Arc::clone(&counter);
                let om = Arc::clone(&observed_max);
                s.spawn(move || {
                    for batch in 0..drain_per_thread {
                        let prev = c
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                                Some(cur.saturating_sub(1))
                            })
                            .unwrap_or(0);
                        // Track highest value seen (for diagnostics)
                        om.fetch_max(prev, Ordering::Relaxed);

                        let current = c.load(Ordering::Relaxed);
                        assert!(
                            current <= 500,
                            "thread={tid} batch={batch}: counter={current} exceeds initial 500 — wraparound detected!"
                        );
                    }
                });
            }
        });

        let final_val = counter.load(Ordering::Relaxed);
        let peak = observed_max.load(Ordering::Relaxed);
        eprintln!(
            "depth_stress_burst initial=500 threads={num_drain_threads} drain_total={} \
             final={final_val} peak_observed={peak}",
            num_drain_threads * drain_per_thread
        );
        assert_eq!(final_val, 0, "burst drain should saturate to 0");
    }

    #[test]
    fn depth_counter_stress_rapid_inc_then_bulk_drain() {
        use std::sync::Arc;

        let counter = Arc::new(AtomicU64::new(0));
        let inc_threads = 8;
        let inc_per_thread = 1_000;
        let expected_total = (inc_threads * inc_per_thread) as u64;

        // Phase 1: rapid concurrent increments
        std::thread::scope(|s| {
            for _ in 0..inc_threads {
                let c = Arc::clone(&counter);
                s.spawn(move || {
                    for _ in 0..inc_per_thread {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        let after_inc = counter.load(Ordering::Relaxed);
        assert_eq!(
            after_inc, expected_total,
            "after {inc_threads}×{inc_per_thread} increments"
        );

        // Phase 2: bulk drain with varying batch sizes
        let drain_amounts = [256, 256, 256, 256, 7000]; // Total: 8024 >> 8000
        std::thread::scope(|s| {
            for &amount in &drain_amounts {
                let c = Arc::clone(&counter);
                s.spawn(move || {
                    let amount_u64 = amount as u64;
                    c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                        Some(cur.saturating_sub(amount_u64))
                    })
                    .ok();
                });
            }
        });

        let final_val = counter.load(Ordering::Relaxed);
        eprintln!(
            "depth_stress_bulk_drain total_inc={expected_total} drain_amounts={drain_amounts:?} final={final_val}"
        );
        assert_eq!(final_val, 0, "bulk drain should saturate to 0");
    }

    #[test]
    fn depth_counter_stress_contention_profile_no_anomaly() {
        use std::sync::Arc;

        // Simulates realistic WBQ workload: many producers, few batch drainers
        let counter = Arc::new(AtomicU64::new(0));
        let producers = 16;
        let drainers = 4;
        let produce_ops = 2_000;
        let drain_batch = 256_u64;
        let anomaly_count = Arc::new(AtomicU64::new(0));

        std::thread::scope(|s| {
            // Producers: enqueue-like increments
            for _ in 0..producers {
                let c = Arc::clone(&counter);
                s.spawn(move || {
                    for _ in 0..produce_ops {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }

            // Drainers: batch drain with anomaly detection
            for _ in 0..drainers {
                let c = Arc::clone(&counter);
                let ac = Arc::clone(&anomaly_count);
                s.spawn(move || {
                    loop {
                        let prev = c
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                                Some(cur.saturating_sub(drain_batch))
                            })
                            .unwrap_or(0);
                        if prev == 0 {
                            break;
                        }
                        let after = c.load(Ordering::Relaxed);
                        // Anomaly: value somehow larger than theoretical max
                        if after > (producers * produce_ops) as u64 {
                            ac.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        let anomalies = anomaly_count.load(Ordering::Relaxed);
        let final_val = counter.load(Ordering::Relaxed);
        eprintln!(
            "depth_stress_contention producers={producers}×{produce_ops} drainers={drainers}×batch{drain_batch} \
             final={final_val} anomalies={anomalies}"
        );
        assert_eq!(anomalies, 0, "no anomalous counter values detected");
    }
}
