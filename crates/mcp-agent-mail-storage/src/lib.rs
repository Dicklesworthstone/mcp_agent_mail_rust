#![forbid(unsafe_code)]
#![allow(clippy::collapsible_if)]
//! Git archive storage layer for MCP Agent Mail.
//!
//! Provides per-project git archives with:
//! - Archive root initialization + per-project git repos + `.gitattributes`
//! - Advisory file locks (`.archive.lock`) and commit locks
//! - Commit queue with batching to reduce lock contention
//! - Message write pipeline (canonical + inbox/outbox copies)
//! - File reservation artifact writes (`sha1(pattern).json` + `id-{id}.json`)
//! - Agent profile writes
//! - Notification signals

pub mod recovery;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Component, Path, PathBuf};
// `Stdio` is referenced fully-qualified in the two remaining sites
// (lines ~1290); removed the bare `use` after bead C5 deleted the
// read-tree shell-out.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use git2::{ErrorCode, IndexAddOption, Repository, Signature};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::Digest as Sha1Digest;
use thiserror::Error;

use mcp_agent_mail_core::{
    LockLevel, OrderedMutex, OrderedRwLock,
    config::{self, Config},
};

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

/// Borrowed input for writing a batch of message bundles with one archive commit.
#[derive(Debug, Clone, Copy)]
pub struct MessageBundleBatchEntry<'a> {
    pub message: &'a serde_json::Value,
    pub body_md: &'a str,
    pub sender: &'a str,
    pub recipients: &'a [String],
    pub extra_paths: &'a [String],
}

/// Metadata included in notification signal files when enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationMessage {
    pub id: Option<i64>,
    pub from: Option<String>,
    pub subject: Option<String>,
    pub importance: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEmitOutcome {
    Emitted,
    Disabled,
    InvalidTarget,
    Debounced,
    WriteFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalClearOutcome {
    Cleared,
    Disabled,
    InvalidTarget,
    Missing,
    RemoveFailed,
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
    sender: Mutex<Option<std::sync::mpsc::SyncSender<WbqMsg>>>,
    drain_handle: OrderedMutex<Option<std::thread::JoinHandle<()>>>,
    lifecycle: Mutex<()>,
    op_depth: Arc<AtomicU64>,
}

// WBQ timing defaults. Capacity and batch limits come from `Config`.
const WBQ_FLUSH_INTERVAL_MS: u64 = 100;
const WBQ_ENQUEUE_TIMEOUT_MS: u64 = 100;
const WBQ_ENQUEUE_MAX_BACKOFF_MS: u64 = 8;

static WBQ: OnceLock<WriteBehindQueue> = OnceLock::new();

fn new_write_behind_queue() -> WriteBehindQueue {
    let op_depth = Arc::new(AtomicU64::new(0));
    let channel_capacity = Config::get().wbq_channel_capacity;
    mcp_agent_mail_core::global_metrics()
        .storage
        .wbq_capacity
        .set(u64::try_from(channel_capacity).unwrap_or(u64::MAX));

    WriteBehindQueue {
        sender: Mutex::new(None),
        drain_handle: OrderedMutex::new(LockLevel::StorageWbqDrainHandle, None),
        lifecycle: Mutex::new(()),
        op_depth,
    }
}

fn wbq_start_inner(wbq: &WriteBehindQueue) {
    let _lifecycle = wbq.lifecycle.lock().unwrap_or_else(|e| e.into_inner());

    let should_spawn = {
        let handle = wbq.drain_handle.lock();
        handle
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    };
    if !should_spawn {
        return;
    }

    if let Some(old_handle) = {
        let mut handle = wbq.drain_handle.lock();
        handle.take()
    } {
        let _ = old_handle.join();
    }

    let config = Config::get();
    let channel_capacity = config.wbq_channel_capacity;
    let drain_batch_cap = config.wbq_drain_batch_cap;
    let (tx, rx) = std::sync::mpsc::sync_channel(channel_capacity);
    let op_depth_worker = Arc::clone(&wbq.op_depth);
    let handle = std::thread::Builder::new()
        .name("wbq-drain".into())
        .spawn(move || wbq_drain_loop(rx, op_depth_worker, channel_capacity, drain_batch_cap))
        .unwrap_or_else(|error| panic!("failed to spawn wbq-drain thread: {error}"));

    *wbq.sender.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    *wbq.drain_handle.lock() = Some(handle);
}

fn wbq_sender_clone(wbq: &WriteBehindQueue) -> Option<std::sync::mpsc::SyncSender<WbqMsg>> {
    wbq.sender.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Spawn the WBQ drain thread. Safe to call multiple times.
pub fn wbq_start() {
    let wbq = WBQ.get_or_init(new_write_behind_queue);
    wbq_start_inner(wbq);
}

fn wbq_record_enqueue_success(op_depth: &AtomicU64) {
    let metrics = mcp_agent_mail_core::global_metrics();
    metrics.storage.wbq_enqueued_total.inc();

    let depth = op_depth.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    metrics.storage.wbq_depth.set(depth);
    metrics.storage.wbq_peak_depth.fetch_max(depth);

    let cap = u64::try_from(Config::get().wbq_channel_capacity).unwrap_or(u64::MAX);
    let threshold = cap.saturating_mul(80).saturating_div(100);
    if threshold > 0 && depth >= threshold {
        if metrics.storage.wbq_over_80_since_us.load() == 0 {
            metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
        }
    } else {
        metrics.storage.wbq_over_80_since_us.set(0);
    }
}

fn wbq_enqueue_with_sender(
    sender: &std::sync::mpsc::SyncSender<WbqMsg>,
    op_depth: &AtomicU64,
    op: WriteOp,
) -> WbqEnqueueResult {
    let envelope = WbqOpEnvelope {
        enqueued_at: Instant::now(),
        op: Box::new(op),
    };

    let msg = WbqMsg::Op(envelope);
    match sender.try_send(msg) {
        Ok(()) => {
            wbq_record_enqueue_success(op_depth);
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
            // try_send + bounded exponential backoff until a deadline.
            let deadline = Instant::now() + Duration::from_millis(WBQ_ENQUEUE_TIMEOUT_MS);
            let mut cur = msg;
            let mut backoff = Duration::from_millis(1);
            let max_backoff = Duration::from_millis(WBQ_ENQUEUE_MAX_BACKOFF_MS);
            loop {
                match sender.try_send(cur) {
                    Ok(()) => {
                        wbq_record_enqueue_success(op_depth);
                        break WbqEnqueueResult::Enqueued;
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        break WbqEnqueueResult::QueueUnavailable;
                    }
                    Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                        let now = Instant::now();
                        if now >= deadline {
                            break WbqEnqueueResult::QueueUnavailable;
                        }
                        let remaining = deadline.saturating_duration_since(now);
                        std::thread::sleep(backoff.min(remaining));
                        backoff = backoff
                            .checked_mul(2)
                            .unwrap_or(max_backoff)
                            .min(max_backoff);
                        cur = returned;
                    }
                }
            }
        }
    }
}

fn wbq_enqueue_with_sender_and_pressure(
    sender: &std::sync::mpsc::SyncSender<WbqMsg>,
    op_depth: &AtomicU64,
    op: WriteOp,
    disk_pressure_level: u64,
) -> WbqEnqueueResult {
    if disk_pressure_level >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64() {
        return WbqEnqueueResult::SkippedDiskCritical;
    }
    wbq_enqueue_with_sender(sender, op_depth, op)
}

/// Enqueue a write op to the background drain thread.
///
/// The DB remains the source of truth; archive writes are best-effort.
pub fn wbq_enqueue(op: WriteOp) -> WbqEnqueueResult {
    let disk_pressure = mcp_agent_mail_core::global_metrics()
        .system
        .disk_pressure_level
        .load();

    let wbq = WBQ.get_or_init(new_write_behind_queue);
    wbq_start_inner(wbq);
    let Some(sender) = wbq_sender_clone(wbq) else {
        return WbqEnqueueResult::QueueUnavailable;
    };
    wbq_enqueue_with_sender_and_pressure(&sender, wbq.op_depth.as_ref(), op, disk_pressure)
}

/// Execute a write op synchronously on the caller thread.
///
/// This is intended as a durability fallback when the write-behind queue is
/// unavailable. The operation still uses the normal storage write path,
/// including retries and async git commit enqueueing where applicable.
pub fn write_op_sync(op: &WriteOp) -> Result<()> {
    wbq_execute_op(op)
}

/// Block until all pending write ops have been drained.
///
/// Uses a blocking `send` so the Flush message is guaranteed to enter the
/// channel even when it is temporarily full.  If the drain thread has
/// panicked (receiver dropped), `send` returns `Err` immediately – no
/// deadlock risk.
pub fn wbq_flush() {
    if let Some(wbq) = WBQ.get() {
        let Some(sender) = wbq_sender_clone(wbq) else {
            return;
        };
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        if sender.send(WbqMsg::Flush(done_tx)).is_ok() {
            match done_rx.recv_timeout(Duration::from_secs(30)) {
                Ok(()) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!("wbq_flush timed out after 30s; drain thread may be stuck");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("wbq_flush: drain thread channel disconnected");
                }
            }
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
        let _lifecycle = wbq.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = wbq_sender_clone(wbq) {
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            if sender.send(WbqMsg::Flush(done_tx)).is_ok() {
                match done_rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        tracing::warn!("wbq_flush timed out after 30s; drain thread may be stuck");
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!("wbq_flush: drain thread channel disconnected");
                    }
                }
            }
            // Tell the drain thread to exit.  Use blocking `send` so it is
            // guaranteed to be delivered (same rationale as wbq_flush).
            let _ = sender.send(WbqMsg::Shutdown);
        }
        *wbq.sender.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

fn wbq_drain_loop(
    rx: std::sync::mpsc::Receiver<WbqMsg>,
    op_depth: Arc<AtomicU64>,
    channel_capacity: usize,
    drain_batch_cap: usize,
) {
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
        while batch.len() < drain_batch_cap {
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
        let cap = u64::try_from(channel_capacity).unwrap_or(u64::MAX);
        let threshold = cap.saturating_mul(80).saturating_div(100);
        if threshold > 0 && depth_after >= threshold {
            if metrics.storage.wbq_over_80_since_us.load() == 0 {
                metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
            }
        } else {
            metrics.storage.wbq_over_80_since_us.set(0);
        }

        let mut errors = 0usize;
        let mut idx = 0usize;
        while idx < batch.len() {
            let end = wbq_message_bundle_batch_end(&batch, idx);
            let envelopes = &batch[idx..end];
            let result = if envelopes.len() > 1 {
                wbq_execute_message_bundle_batch(envelopes)
            } else {
                wbq_execute_op(&envelopes[0].op)
            };
            for envelope in envelopes {
                let latency_us = u64::try_from(
                    envelope
                        .enqueued_at
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)),
                )
                .unwrap_or(u64::MAX);
                metrics.storage.wbq_queue_latency_us.record(latency_us);
            }
            if let Err(error) = result {
                if envelopes.len() > 1 {
                    tracing::warn!(
                        "[wbq-drain] batched message-bundle run failed ({} ops): {error}",
                        envelopes.len()
                    );
                    errors += envelopes.len();
                } else {
                    tracing::warn!("[wbq-drain] op failed: {error}");
                    errors += 1;
                }
            }
            idx = end;
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
                let cap = u64::try_from(channel_capacity).unwrap_or(u64::MAX);
                let threshold = cap.saturating_mul(80).saturating_div(100);
                if threshold > 0 && depth_after >= threshold {
                    if metrics.storage.wbq_over_80_since_us.load() == 0 {
                        metrics.storage.wbq_over_80_since_us.set(now_micros_u64());
                    }
                } else {
                    metrics.storage.wbq_over_80_since_us.set(0);
                }

                let r = wbq_execute_op(&envelope.op);
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

fn message_bundle_batch_group_key(op: &WriteOp) -> Option<(&str, &Path, &str, &str)> {
    match op {
        WriteOp::MessageBundle {
            project_slug,
            config,
            ..
        } => Some((
            project_slug.as_str(),
            config.storage_root.as_path(),
            config.git_author_name.as_str(),
            config.git_author_email.as_str(),
        )),
        _ => None,
    }
}

fn wbq_message_bundle_batch_end(batch: &[WbqOpEnvelope], start: usize) -> usize {
    let Some(anchor_key) = message_bundle_batch_group_key(batch[start].op.as_ref()) else {
        return start + 1;
    };

    let mut end = start + 1;
    while end < batch.len() {
        if message_bundle_batch_group_key(batch[end].op.as_ref()) != Some(anchor_key) {
            break;
        }
        end += 1;
    }
    end
}

fn wbq_execute_message_bundle_batch(envelopes: &[WbqOpEnvelope]) -> Result<()> {
    debug_assert!(!envelopes.is_empty());

    let mut attempts = 0;
    loop {
        match wbq_execute_message_bundle_batch_inner(envelopes) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                if attempts >= 3 {
                    return Err(e);
                }
                match &e {
                    StorageError::Io(_)
                    | StorageError::LockContention { .. }
                    | StorageError::GitIndexLock { .. }
                    | StorageError::LockTimeout(_) => {
                        tracing::warn!(
                            "[wbq-drain] batch failed (attempt {attempts}/3): {e}, retrying..."
                        );
                        std::thread::sleep(Duration::from_millis(50 * (1 << attempts)));
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

fn wbq_execute_message_bundle_batch_inner(envelopes: &[WbqOpEnvelope]) -> Result<()> {
    let Some(WriteOp::MessageBundle {
        project_slug,
        config,
        ..
    }) = envelopes.first().map(|envelope| envelope.op.as_ref())
    else {
        return Err(StorageError::Io(std::io::Error::other(
            "wbq message-bundle batch requires at least one message-bundle op",
        )));
    };

    let archive = ensure_archive(config, project_slug)?;
    let batch_entries = envelopes
        .iter()
        .map(|envelope| match envelope.op.as_ref() {
            WriteOp::MessageBundle {
                message_json,
                body_md,
                sender,
                recipients,
                extra_paths,
                ..
            } => MessageBundleBatchEntry {
                message: message_json,
                body_md,
                sender,
                recipients,
                extra_paths,
            },
            _ => unreachable!("message-bundle batch slices are grouped before execution"),
        })
        .collect::<Vec<_>>();
    write_message_batch_bundle(&archive, config, &batch_entries, None)
}

fn wbq_execute_op(op: &WriteOp) -> Result<()> {
    let mut attempts = 0;
    loop {
        match wbq_execute_op_inner(op) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                if attempts >= 3 {
                    return Err(e);
                }
                match &e {
                    StorageError::Io(_)
                    | StorageError::LockContention { .. }
                    | StorageError::GitIndexLock { .. }
                    | StorageError::LockTimeout(_) => {
                        tracing::warn!(
                            "[wbq-drain] op failed (attempt {attempts}/3): {e}, retrying..."
                        );
                        std::thread::sleep(Duration::from_millis(50 * (1 << attempts)));
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

fn wbq_execute_op_inner(op: &WriteOp) -> Result<()> {
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
        } => match emit_notification_signal(config, project_slug, agent_name, metadata.as_ref()) {
            SignalEmitOutcome::Emitted
            | SignalEmitOutcome::Disabled
            | SignalEmitOutcome::InvalidTarget
            | SignalEmitOutcome::Debounced => Ok(()),
            SignalEmitOutcome::WriteFailed => {
                Err(StorageError::Io(std::io::Error::other(format!(
                    "failed to emit notification signal for project={project_slug} agent={agent_name}"
                ))))
            }
        },
        WriteOp::ClearSignal {
            config,
            project_slug,
            agent_name,
        } => match clear_notification_signal(config, project_slug, agent_name) {
            SignalClearOutcome::Cleared
            | SignalClearOutcome::Disabled
            | SignalClearOutcome::InvalidTarget
            | SignalClearOutcome::Missing => Ok(()),
            SignalClearOutcome::RemoveFailed => {
                Err(StorageError::Io(std::io::Error::other(format!(
                    "failed to clear notification signal for project={project_slug} agent={agent_name}"
                ))))
            }
        },
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
type ArchiveProcessLockKey = PathBuf;

static ARCHIVE_LOCK_MAP: OnceLock<OrderedRwLock<HashMap<ArchiveProcessLockKey, Arc<Mutex<()>>>>> =
    OnceLock::new();

fn archive_process_lock_for_root(root: PathBuf) -> Arc<Mutex<()>> {
    let map = ARCHIVE_LOCK_MAP
        .get_or_init(|| OrderedRwLock::new(LockLevel::StorageArchiveLockMap, HashMap::new()));
    let key = root;

    // Fast path: read lock for existing entry.
    {
        let guard = map.read();
        if let Some(lock) = guard.get(&key) {
            return Arc::clone(lock);
        }
    }

    // Slow path: write lock to insert.
    let mut guard = map.write();
    if let Some(lock) = guard.get(&key) {
        return Arc::clone(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    guard.insert(key, Arc::clone(&lock));
    lock
}

fn archive_process_lock(archive: &ProjectArchive) -> Result<Arc<Mutex<()>>> {
    Ok(archive_process_lock_for_root(
        archive_project_root_canonical_checked(archive)?,
    ))
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

            // Mix thread-id string hash with timestamp to ensure per-thread jitter.
            // Simple FNV-1a style mixer on the debug string representation of ThreadId.
            let tid_str = format!("{tid:?}");
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in tid_str.bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }

            let mut seed = now ^ h;
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
    /// Retained file handle that holds the OS-level flock.
    /// Must live as long as the lock is held; dropping releases the flock.
    lock_file: Option<fs::File>,
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
            lock_file: None,
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

    fn validate_paths(&self) -> Result<()> {
        for (label, path) in [
            ("lock path", self.path.as_path()),
            ("lock metadata path", self.metadata_path.as_path()),
        ] {
            if path_existing_prefix_has_symlink(path)? {
                return Err(StorageError::InvalidPath(format!(
                    "{label} must not include symlinks: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Acquire the lock with retry and stale detection.
    pub fn acquire(&mut self) -> Result<()> {
        use fs2::FileExt;

        let start = Instant::now();

        self.validate_paths()?;
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
                    // Verify lock identity to prevent race with cleanup_if_stale
                    if !self.verify_lock_identity(&file)? {
                        let _ = file.unlock();
                        continue;
                    }

                    // Lock acquired - retain file handle to hold the OS-level flock
                    self.write_metadata()?;
                    self.lock_file = Some(file);
                    self.held = true;
                    return Ok(());
                }
                Err(_) => {
                    // Lock held by another process - check for stale owner first.
                    if self.cleanup_if_stale()? {
                        // Stale lock cleaned up; retry immediately without backoff.
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
                    let jitter_range = base_ms / 2 + 1; // 50% range for ±25%
                    let jitter = thread_jitter_ms(jitter_range);
                    let sleep_ms = base_ms
                        .saturating_sub(base_ms / 4)
                        .saturating_add(jitter)
                        .max(10);
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

        // Remove files BEFORE releasing flock to prevent race where another
        // process acquires the lock between flock release and file removal.
        let _ = fs::remove_file(&self.metadata_path);
        let _ = fs::remove_file(&self.path);
        // Now drop the file handle to release the OS-level flock
        self.lock_file = None;
        Ok(())
    }

    /// Write owner metadata alongside the lock file.
    fn write_metadata(&self) -> Result<()> {
        self.validate_paths()?;
        let meta = LockOwnerMeta {
            pid: std::process::id(),
            created_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
        };
        write_json(&self.metadata_path, &serde_json::to_value(&meta)?, false)
    }

    /// Verify that the locked file handle corresponds to the file currently at `self.path`.
    fn verify_lock_identity(&self, file: &fs::File) -> Result<bool> {
        let file_meta = file.metadata()?;
        let path_meta = match fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_symlink() => return Ok(false),
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(file_meta.ino() == path_meta.ino())
        }
        #[cfg(not(unix))]
        {
            // Without inode comparison we cannot detect the rename-under-flock race.
            // Accept the lock; flock itself provides mutual exclusion.
            Ok(true)
        }
    }

    /// Check whether this lock artifact is stale and remove it when safe.
    ///
    /// Safety rules:
    /// - Never remove a lock file unless we can first acquire an exclusive flock
    ///   on the current inode (prevents deleting an actively-held lock).
    /// - Consider stale when owner PID is dead, or lock age exceeds `stale_timeout`
    ///   (when `stale_timeout > 0`).
    /// - Return `Ok(false)` for benign races/permission issues so callers can retry.
    fn cleanup_if_stale(&self) -> Result<bool> {
        use fs2::FileExt;

        self.validate_paths()?;
        if !self.path.exists() {
            return Ok(false);
        }

        let file = match fs::OpenOptions::new().write(true).open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        };
        if file.try_lock_exclusive().is_err() {
            return Ok(false);
        }

        // Ensure we are still looking at the same file currently mounted at `self.path`.
        if !self.verify_lock_identity(&file)? {
            let _ = file.unlock();
            return Ok(false);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let meta = match fs::symlink_metadata(&self.metadata_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::read_to_string(&self.metadata_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<LockOwnerMeta>(&s).ok())
            }
            Ok(_) => None,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
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
        } else if self.stale_timeout.is_zero() {
            false
        } else {
            age.is_some_and(|a| a >= self.stale_timeout.as_secs_f64())
        };

        if !is_stale {
            let _ = file.unlock();
            return Ok(false);
        }

        // Try removing while lock is held (best race safety).
        let removed = match fs::remove_file(&self.path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Windows can require closing/unlocking first before unlinking.
                let _ = file.unlock();
                drop(file);
                fs::remove_file(&self.path).is_ok()
            }
            Err(_) => false,
        };

        if removed {
            let _ = fs::remove_file(&self.metadata_path);
            return Ok(true);
        }

        Ok(false)
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
    let process_lock = archive_process_lock(archive)?;
    let project_root = archive_project_root_checked(archive)?;
    let _guard = process_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut lock = FileLock::new(project_root.join(".archive.lock"));
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

/// Check if a process with the given PID is alive.
///
/// Prefers `/proc/<pid>` on Linux (no fork/exec overhead), falls back to
/// `kill -0` on other Unix platforms.
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Fast path: /proc exists on Linux — avoids fork+exec overhead.
        let proc_path = format!("/proc/{pid}");
        if Path::new("/proc").exists() {
            return Path::new(&proc_path).exists();
        }
        // Fallback for macOS / other Unix without /proc.
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
    #[serde(default)]
    pub batch_size_histogram: BTreeMap<usize, u64>,
    pub queue_size: usize,
    pub errors: usize,
}

fn record_batch_size_samples(
    stats: &mut CommitQueueStats,
    sizes: &mut VecDeque<usize>,
    batch_size: usize,
    sample_count: usize,
) {
    if batch_size == 0 || sample_count == 0 {
        return;
    }

    let added = u64::try_from(sample_count).unwrap_or(u64::MAX);
    let entry = stats.batch_size_histogram.entry(batch_size).or_insert(0);
    *entry = entry.saturating_add(added);

    for _ in 0..sample_count {
        sizes.push_back(batch_size);
    }
    while sizes.len() > 100 {
        sizes.pop_front();
    }

    let avg = if sizes.is_empty() {
        0.0
    } else {
        sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
    };
    stats.avg_batch_size = (avg * 100.0).round() / 100.0;
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
        let repo_root = normalize_repo_root_key(&repo_root);

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
        record_batch_size_samples(&mut stats, &mut sizes, batch_size, 1);
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
#[derive(Clone)]
struct CoalescerCommitFields {
    enqueued_at: Instant,
    enqueued_wall: DateTime<Utc>,
    git_author_name: String,
    git_author_email: String,
    message: String,
    rel_paths: Vec<String>,
}

struct CoalescerSpillRepo {
    pending_requests: u64,
    earliest_enqueued_at: Instant,
    earliest_enqueued_wall: DateTime<Utc>,
    latest_enqueued_wall: DateTime<Utc>,
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
    earliest_enqueued_wall: DateTime<Utc>,
    latest_enqueued_wall: DateTime<Utc>,
    dirty_all: bool,
    paths: Vec<String>,
    git_author_name: String,
    git_author_email: String,
    message_first_lines: Vec<String>,
    message_total: u64,
}

struct CoalescerCommitOutcome {
    committed_requests: u64,
    committed_commits: u64,
    failed_requests: Vec<CoalescerCommitFields>,
}

struct CoalescerSpillOutcome {
    committed_requests: u64,
    committed_commits: u64,
    failed_work: Option<CoalescerSpilledWork>,
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
    /// Stores wake tokens to avoid dropping concurrent wakeups.
    work_cv: Arc<(Mutex<u64>, std::sync::Condvar)>,
    /// Signal workers to shut down.
    shutdown: Arc<AtomicBool>,
    /// Force workers to drain immediately, bypassing the time-based batch window.
    force_flush: Arc<AtomicBool>,
    /// Global stats (backward-compatible aggregate view).
    stats: Arc<Mutex<CommitQueueStats>>,
    pending_requests: Arc<AtomicU64>,
    /// Rolling batch size window for avg_batch_size calculation.
    _batch_sizes: Arc<Mutex<VecDeque<usize>>>,
    /// Number of worker threads spawned.
    worker_count: usize,
}

// Coalescer defaults. Queue and worker limits still come from `Config`.
/// Default number of archive events to coalesce before committing.
pub const DEFAULT_ARCHIVE_BATCH_EVENTS: usize = 32;
/// Default archive batch flush interval in milliseconds.
pub const DEFAULT_ARCHIVE_BATCH_MS: u64 = 1_000;
/// Backward-compatible alias for callers that still reference the old constant.
pub const DEFAULT_COALESCER_FLUSH_MS: u64 = DEFAULT_ARCHIVE_BATCH_MS;
/// Guardrail against accidental zero-sized batches.
const MIN_ARCHIVE_BATCH_EVENTS: usize = 1;
/// Guardrail against accidental zero-duration flush intervals.
const MIN_COALESCER_FLUSH_MS: u64 = 5;
const MAX_COALESCER_FLUSH_MS: u64 = 5_000;
const MIN_COALESCER_FLUSH_INTERVAL: Duration = Duration::from_millis(MIN_COALESCER_FLUSH_MS);

const COMMIT_COALESCER_SOFT_CAP: u64 = 8_192;

const COALESCER_SPILL_PATH_CAP: usize = 4_096;
const COALESCER_SPILL_MESSAGE_CAP: usize = 32;
const COALESCER_SPILL_MESSAGE_LINE_MAX_CHARS: usize = 120;

#[inline]
fn clamp_coalescer_flush_interval(interval: Duration) -> Duration {
    interval.max(MIN_COALESCER_FLUSH_INTERVAL)
}

fn parse_archive_batch_usize(primary_key: &str, legacy_key: &str, default: usize) -> usize {
    config::env_value(primary_key)
        .or_else(|| config::env_value(legacy_key))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_archive_batch_u64(primary_key: &str, legacy_key: &str, default: u64) -> u64 {
    config::env_value(primary_key)
        .or_else(|| config::env_value(legacy_key))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn configured_coalescer_flush_interval() -> Duration {
    let ms = parse_archive_batch_u64(
        "AM_ARCHIVE_BATCH_MS",
        "AM_COALESCER_FLUSH_MS",
        DEFAULT_ARCHIVE_BATCH_MS,
    )
    .clamp(MIN_COALESCER_FLUSH_MS, MAX_COALESCER_FLUSH_MS);
    Duration::from_millis(ms)
}

fn configured_coalescer_batch_size() -> usize {
    parse_archive_batch_usize(
        "AM_ARCHIVE_BATCH_EVENTS",
        "AM_COALESCER_MAX_BATCH_SIZE",
        DEFAULT_ARCHIVE_BATCH_EVENTS,
    )
    .clamp(MIN_ARCHIVE_BATCH_EVENTS, COMMIT_COALESCER_SOFT_CAP as usize)
}

/// Auto-detect worker count bounded by `Config::coalescer_max_workers`.
///
/// When the configured max is `1`, honor that explicitly instead of trying to
/// apply the usual multi-worker floor of `2`.
fn coalescer_worker_count() -> usize {
    let max_workers = Config::get().coalescer_max_workers;
    if max_workers <= 1 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, max_workers)
}

impl CommitCoalescer {
    /// Create a new coalescer and spawn the worker pool.
    pub fn new(flush_interval: Duration) -> Self {
        let flush_interval = clamp_coalescer_flush_interval(flush_interval);
        let stats = Arc::new(Mutex::new(CommitQueueStats::default()));
        let batch_sizes = Arc::new(Mutex::new(VecDeque::new()));
        let pending_requests = Arc::new(AtomicU64::new(0));
        let repos: Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let work_cv = Arc::new((Mutex::new(0_u64), std::sync::Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let force_flush = Arc::new(AtomicBool::new(false));

        let worker_count = coalescer_worker_count();

        for worker_idx in 0..worker_count {
            let w_repos = Arc::clone(&repos);
            let w_cv = Arc::clone(&work_cv);
            let w_shutdown = Arc::clone(&shutdown);
            let w_force_flush = Arc::clone(&force_flush);
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
                        w_force_flush,
                        w_stats,
                        w_sizes,
                        w_pending,
                        flush_interval,
                        worker_count,
                    );
                })
                .unwrap_or_else(|error| {
                    panic!("failed to spawn commit-coalescer-{worker_idx} thread: {error}")
                });
        }

        mcp_agent_mail_core::global_metrics()
            .storage
            .commit_soft_cap
            .set(COMMIT_COALESCER_SOFT_CAP);

        Self {
            repos,
            work_cv,
            shutdown,
            force_flush,
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
            earliest_enqueued_wall: fields.enqueued_wall,
            latest_enqueued_wall: fields.enqueued_wall,
            dirty_all: false,
            paths: BTreeSet::new(),
            git_author_name: fields.git_author_name.clone(),
            git_author_email: fields.git_author_email.clone(),
            message_first_lines: VecDeque::new(),
            message_total: 0,
        });

        entry.pending_requests = entry.pending_requests.saturating_add(1);
        entry.message_total = entry.message_total.saturating_add(1);
        rq.depth.fetch_add(1, Ordering::Relaxed);
        if fields.enqueued_at < entry.earliest_enqueued_at {
            entry.earliest_enqueued_at = fields.enqueued_at;
        }
        if fields.enqueued_wall < entry.earliest_enqueued_wall {
            entry.earliest_enqueued_wall = fields.enqueued_wall;
        }
        if fields.enqueued_wall > entry.latest_enqueued_wall {
            entry.latest_enqueued_wall = fields.enqueued_wall;
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
        let repo_root = normalize_repo_root_key(&repo_root);

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
            enqueued_wall: Utc::now(),
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
        let repo_queue_cap = config.coalescer_queue_cap;
        let queue_depth = rq.depth.load(Ordering::Relaxed);
        if queue_depth < repo_queue_cap as u64 {
            let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
            // Re-check under lock
            if q.len() < repo_queue_cap {
                q.push_back(fields);
                rq.depth.fetch_add(1, Ordering::Relaxed);
                drop(q);
            } else {
                drop(q);
                Self::spill_to_repo(&rq, fields);
            }
        } else {
            Self::spill_to_repo(&rq, fields);
        }

        // Wake a worker
        let (lock, cvar) = &*self.work_cv;
        {
            let mut wake_tokens = lock.lock().unwrap_or_else(|e| e.into_inner());
            *wake_tokens = wake_tokens.saturating_add(1).min(self.worker_count as u64);
        }
        cvar.notify_one();
    }

    /// Block until all pending commits are flushed to git.
    ///
    /// Use this in tests or during graceful shutdown to ensure all queued
    /// commits are persisted before proceeding.
    pub fn flush_sync(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        self.force_flush.store(true, Ordering::Release);

        loop {
            // Wake all workers
            {
                let (lock, cvar) = &*self.work_cv;
                let mut wake_tokens = lock.lock().unwrap_or_else(|e| e.into_inner());
                *wake_tokens = wake_tokens
                    .saturating_add(self.worker_count as u64)
                    .min(self.worker_count as u64);
                cvar.notify_all();
            }

            // Check if all repos are empty and not being processed
            let all_empty = {
                if self.pending_requests.load(Ordering::Relaxed) > 0 {
                    false
                } else {
                    let repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
                    repos.values().all(|rq| {
                        rq.depth.load(Ordering::Relaxed) == 0
                            && !rq.processing.load(Ordering::Relaxed)
                    })
                }
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

        self.force_flush.store(false, Ordering::Release);
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
        // Drain queued commits before worker shutdown so scoped/test instances
        // preserve the same graceful-shutdown contract as the server signal path.
        self.flush_sync();
        self.shutdown.store(true, Ordering::Release);
        let (_, cvar) = &*self.work_cv;
        cvar.notify_all();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalescerRepoReadiness {
    Ready,
    Waiting(Duration),
    Empty,
}

fn coalescer_repo_readiness(
    rq: &RepoQueue,
    max_batch_size: usize,
    flush_interval: Duration,
    force_flush: bool,
) -> CoalescerRepoReadiness {
    let depth = rq.depth.load(Ordering::Relaxed);
    if depth == 0 {
        return CoalescerRepoReadiness::Empty;
    }
    if force_flush || depth >= u64::try_from(max_batch_size).unwrap_or(u64::MAX) {
        return CoalescerRepoReadiness::Ready;
    }

    let mut earliest: Option<Instant> = None;
    {
        let q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= max_batch_size {
            return CoalescerRepoReadiness::Ready;
        }
        if let Some(front) = q.front() {
            earliest = Some(front.enqueued_at);
        }
    }

    {
        let spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(repo) = spill.inner.as_ref() {
            if repo.pending_requests >= u64::try_from(max_batch_size).unwrap_or(u64::MAX) {
                return CoalescerRepoReadiness::Ready;
            }
            earliest = match earliest {
                Some(current) => Some(current.min(repo.earliest_enqueued_at)),
                None => Some(repo.earliest_enqueued_at),
            };
        }
    }

    let Some(earliest) = earliest else {
        return CoalescerRepoReadiness::Empty;
    };

    let elapsed = earliest.elapsed();
    if elapsed >= flush_interval {
        CoalescerRepoReadiness::Ready
    } else {
        CoalescerRepoReadiness::Waiting(flush_interval - elapsed)
    }
}

fn coalescer_wait_for_due_work(
    work_cv: &Arc<(Mutex<u64>, std::sync::Condvar)>,
    shutdown: &Arc<AtomicBool>,
    wait_for: Duration,
) {
    let (lock, cvar) = &**work_cv;
    let mut wake_tokens = lock.lock().unwrap_or_else(|e| e.into_inner());
    if *wake_tokens == 0 && !shutdown.load(Ordering::Relaxed) {
        let (guard, _) = cvar
            .wait_timeout(wake_tokens, wait_for)
            .unwrap_or_else(|e| e.into_inner());
        wake_tokens = guard;
    }
    if *wake_tokens > 0 {
        *wake_tokens -= 1;
    }
}

/// Worker thread for the per-repo commit coalescer pool.
///
/// Strategy:
/// 1. Wait on condvar (with a bounded idle timeout as a safety probe)
/// 2. Scan all repos; pick the one with lowest last_serviced_us that has depth > 0
///    and is not currently being processed by another worker (CAS lock)
/// 3. Drain its queue + spill (up to batch size)
/// 4. Commit batch for that single repo
/// 5. Update per-repo and global metrics
/// 6. If more work remains across any repo, re-signal condvar
/// 7. Repeat
#[allow(clippy::too_many_arguments)]
fn coalescer_pool_worker(
    repos: Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>>,
    work_cv: Arc<(Mutex<u64>, std::sync::Condvar)>,
    shutdown: Arc<AtomicBool>,
    force_flush: Arc<AtomicBool>,
    stats: Arc<Mutex<CommitQueueStats>>,
    batch_sizes: Arc<Mutex<VecDeque<usize>>>,
    pending_requests: Arc<AtomicU64>,
    flush_interval: Duration,
    worker_count: usize,
) {
    // Reduce idle wakeups while keeping periodic safety probes in case a
    // notification is lost under extreme contention.
    let idle_wait = flush_interval.max(Duration::from_secs(1));
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // Phase 1: Wait for work
        {
            let (lock, cvar) = &*work_cv;
            let mut wake_tokens = lock.lock().unwrap_or_else(|e| e.into_inner());
            while *wake_tokens == 0 && !shutdown.load(Ordering::Relaxed) {
                let (guard, _) = cvar
                    .wait_timeout(wake_tokens, idle_wait)
                    .unwrap_or_else(|e| e.into_inner());
                wake_tokens = guard;
            }
            if *wake_tokens > 0 {
                *wake_tokens -= 1;
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // Phase 2: Pick a repo via LRS (least-recently-serviced) scheduling
        loop {
            let max_batch_size = configured_coalescer_batch_size();
            let force_now = force_flush.load(Ordering::Acquire);
            let mut next_due: Option<Duration> = None;
            let chosen: Option<(PathBuf, Arc<RepoQueue>)> = {
                let repos_guard = repos.lock().unwrap_or_else(|e| e.into_inner());
                let mut best: Option<(PathBuf, Arc<RepoQueue>, u64)> = None;
                for (path, rq) in repos_guard.iter() {
                    // Skip repos already being processed by another worker
                    if rq.processing.load(Ordering::Relaxed) {
                        continue;
                    }
                    match coalescer_repo_readiness(rq, max_batch_size, flush_interval, force_now) {
                        CoalescerRepoReadiness::Ready => {}
                        CoalescerRepoReadiness::Waiting(wait_for) => {
                            next_due =
                                Some(next_due.map_or(wait_for, |current| current.min(wait_for)));
                            continue;
                        }
                        CoalescerRepoReadiness::Empty => continue,
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
                if let Some(wait_for) = next_due {
                    coalescer_wait_for_due_work(&work_cv, &shutdown, wait_for);
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    continue;
                }
                break; // No more work available; go back to Phase 1
            };

            // CAS: claim exclusive processing of this repo
            if rq
                .processing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                // Another worker beat us to it; try again immediately
                continue;
            }

            // Successfully claimed repo.
            self_process_repo(
                &repo_root,
                &rq,
                &stats,
                &batch_sizes,
                &pending_requests,
                worker_count,
                &repos,
                &work_cv,
            );
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn self_process_repo(
    repo_root: &Path,
    rq: &Arc<RepoQueue>,
    stats: &Arc<Mutex<CommitQueueStats>>,
    batch_sizes: &Arc<Mutex<VecDeque<usize>>>,
    pending_requests: &Arc<AtomicU64>,
    worker_count: usize,
    repos: &Arc<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>>,
    work_cv: &Arc<(Mutex<u64>, std::sync::Condvar)>,
) {
    let max_batch_size = configured_coalescer_batch_size();

    // RAII guard to ensure processing flag is cleared even on panic
    struct ProcessingGuard<'a> {
        rq: &'a Arc<RepoQueue>,
    }
    impl<'a> Drop for ProcessingGuard<'a> {
        fn drop(&mut self) {
            self.rq.processing.store(false, Ordering::Release);
        }
    }
    let _guard = ProcessingGuard { rq };

    // Phase 3: Drain queue + spill for this repo
    let mut batch: Vec<CoalescerCommitFields> = Vec::new();
    let queue_is_empty = {
        let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
        while batch.len() < max_batch_size {
            let next = q.front();
            if let Some(next_fields) = next {
                if let Some(first) = batch.first()
                    && (next_fields.git_author_name != first.git_author_name
                        || next_fields.git_author_email != first.git_author_email)
                {
                    break;
                }
                batch.push(
                    q.pop_front()
                        .expect("pop_front must succeed after front() returned Some"),
                );
            } else {
                break;
            }
        }
        if !batch.is_empty() {
            coalescer_depth_decrement(&rq.depth, batch.len() as u64);
        }
        q.is_empty()
    };

    // Drain spill ONLY if the main queue is completely empty to preserve FIFO order.
    // If the queue still has items, we'll get to the spill on a future iteration.
    let spilled_work = if queue_is_empty {
        coalescer_drain_repo_spill(rq, repo_root)
    } else {
        None
    };

    struct PanicGuard<'a> {
        rq: &'a RepoQueue,
        work_cv: &'a Arc<(Mutex<u64>, std::sync::Condvar)>,
        worker_count: usize,
        pending_batch: Vec<CoalescerCommitFields>,
        inflight_batch: Option<Vec<CoalescerCommitFields>>,
        pending_spilled: Option<CoalescerSpilledWork>,
        inflight_spilled: Option<CoalescerSpilledWork>,
    }
    impl<'a> Drop for PanicGuard<'a> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                if coalescer_restore_drained_work_on_panic(
                    self.rq,
                    &mut self.pending_batch,
                    &mut self.inflight_batch,
                    &mut self.pending_spilled,
                    &mut self.inflight_spilled,
                ) {
                    coalescer_signal_worker(self.work_cv, self.worker_count);
                }
            }
        }
    }
    let mut panic_guard = PanicGuard {
        rq,
        work_cv,
        worker_count,
        pending_batch: batch,
        inflight_batch: None,
        pending_spilled: spilled_work,
        inflight_spilled: None,
    };

    let drained_count = panic_guard.pending_batch.len() as u64
        + panic_guard
            .pending_spilled
            .as_ref()
            .map_or(0, |w| w.pending_requests);

    if drained_count == 0 {
        // Defensive self-heal: if depth and queue contents diverge, reconcile
        // once here to avoid repeatedly selecting an empty repo.
        let _ = coalescer_reconcile_repo_depth(rq);
    }

    // Phase 4: Commit
    while !panic_guard.pending_batch.is_empty() {
        let chunk_size = panic_guard.pending_batch.len().min(max_batch_size);
        let chunk = panic_guard
            .pending_batch
            .drain(..chunk_size)
            .collect::<Vec<_>>();
        panic_guard.inflight_batch = Some(chunk);
        if let Some(chunk) = panic_guard.inflight_batch.as_ref() {
            let outcome = coalescer_commit_batch(repo_root, chunk, stats, batch_sizes);
            let failed_requests = outcome.failed_requests;

            let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            let metrics = mcp_agent_mail_core::global_metrics();
            if outcome.committed_requests > 0 {
                metrics
                    .storage
                    .commit_drained_total
                    .add(outcome.committed_requests);
                rq.metrics
                    .drained_total
                    .fetch_add(outcome.committed_requests, Ordering::Relaxed);
            }
            if outcome.committed_commits > 0 {
                rq.metrics
                    .commits_total
                    .fetch_add(outcome.committed_commits, Ordering::Relaxed);
            }
            if !failed_requests.is_empty() {
                rq.metrics.errors_total.fetch_add(
                    u64::try_from(failed_requests.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }

            if outcome.committed_requests == chunk_len {
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
            }

            if outcome.committed_requests > 0 {
                coalescer_update_pending(pending_requests, outcome.committed_requests);
            }
            if !failed_requests.is_empty() {
                coalescer_requeue_requests(rq, failed_requests);
            }
        }
        panic_guard.inflight_batch = None;
    }

    if let Some(work) = panic_guard.pending_spilled.take() {
        panic_guard.inflight_spilled = Some(work);
    }
    if let Some(work) = panic_guard.inflight_spilled.as_ref() {
        let outcome = coalescer_commit_spilled_work(work, stats, batch_sizes);
        if let Some(failed_work) = outcome.failed_work {
            rq.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
            coalescer_restore_spilled_work(rq, failed_work);
        }

        let metrics = mcp_agent_mail_core::global_metrics();
        if outcome.committed_requests > 0 {
            metrics
                .storage
                .commit_drained_total
                .add(outcome.committed_requests);
            rq.metrics
                .drained_total
                .fetch_add(outcome.committed_requests, Ordering::Relaxed);

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

            coalescer_update_pending(pending_requests, outcome.committed_requests);
        }
        if outcome.committed_commits > 0 {
            rq.metrics
                .commits_total
                .fetch_add(outcome.committed_commits, Ordering::Relaxed);
        }
        panic_guard.inflight_spilled = None;
    }

    // Release processing lock (via guard drop) + update last_serviced timestamp
    rq.last_serviced_us
        .store(now_micros_u64(), Ordering::Relaxed);

    // If any repo still has work, wake another worker
    let more_work = {
        let repos_guard = repos.lock().unwrap_or_else(|e| e.into_inner());
        repos_guard
            .values()
            .any(|r| r.depth.load(Ordering::Relaxed) > 0)
    };
    if more_work {
        coalescer_signal_worker(work_cv, worker_count);
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

fn coalescer_signal_worker(work_cv: &Arc<(Mutex<u64>, std::sync::Condvar)>, worker_count: usize) {
    let (lock, cvar) = &**work_cv;
    {
        let mut wake_tokens = lock.lock().unwrap_or_else(|e| e.into_inner());
        *wake_tokens = wake_tokens.saturating_add(1).min(worker_count as u64);
    }
    cvar.notify_one();
}

fn coalescer_restore_drained_work_on_panic(
    rq: &RepoQueue,
    pending_batch: &mut Vec<CoalescerCommitFields>,
    inflight_batch: &mut Option<Vec<CoalescerCommitFields>>,
    pending_spilled: &mut Option<CoalescerSpilledWork>,
    inflight_spilled: &mut Option<CoalescerSpilledWork>,
) -> bool {
    let mut restored_any = false;
    if !pending_batch.is_empty() {
        coalescer_requeue_requests(rq, std::mem::take(pending_batch));
        restored_any = true;
    }
    if let Some(batch) = inflight_batch.take()
        && !batch.is_empty()
    {
        coalescer_requeue_requests(rq, batch);
        restored_any = true;
    }
    if let Some(work) = pending_spilled.take() {
        coalescer_restore_spilled_work(rq, work);
        restored_any = true;
    }
    if let Some(work) = inflight_spilled.take() {
        coalescer_restore_spilled_work(rq, work);
        restored_any = true;
    }
    restored_any
}

fn coalescer_depth_decrement(depth: &AtomicU64, drained: u64) -> u64 {
    depth
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_sub(drained))
        })
        .unwrap_or(0)
        .saturating_sub(drained)
}

/// Drain a single repo's spill buffer into a `CoalescerSpilledWork`.
fn coalescer_drain_repo_spill(rq: &RepoQueue, repo_root: &Path) -> Option<CoalescerSpilledWork> {
    let mut guard = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
    let repo = guard.inner.take()?;
    if repo.pending_requests == 0 {
        return None;
    }
    coalescer_depth_decrement(&rq.depth, repo.pending_requests);
    Some(CoalescerSpilledWork {
        repo_root: repo_root.to_path_buf(),
        pending_requests: repo.pending_requests,
        earliest_enqueued_at: repo.earliest_enqueued_at,
        earliest_enqueued_wall: repo.earliest_enqueued_wall,
        latest_enqueued_wall: repo.latest_enqueued_wall,
        dirty_all: repo.dirty_all,
        paths: repo.paths.into_iter().collect(),
        git_author_name: repo.git_author_name,
        git_author_email: repo.git_author_email,
        message_first_lines: repo.message_first_lines.into_iter().collect(),
        message_total: repo.message_total,
    })
}

fn coalescer_requeue_requests(rq: &RepoQueue, failed_requests: Vec<CoalescerCommitFields>) {
    if failed_requests.is_empty() {
        return;
    }
    let requeued = u64::try_from(failed_requests.len()).unwrap_or(u64::MAX);
    let mut queue = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
    for request in failed_requests.into_iter().rev() {
        queue.push_front(request);
    }
    rq.depth.fetch_add(requeued, Ordering::Relaxed);
}

fn coalescer_restore_spilled_work(rq: &RepoQueue, work: CoalescerSpilledWork) {
    if work.pending_requests == 0 {
        return;
    }

    let mut spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
    let repo = spill.inner.get_or_insert_with(|| CoalescerSpillRepo {
        pending_requests: 0,
        earliest_enqueued_at: work.earliest_enqueued_at,
        earliest_enqueued_wall: work.earliest_enqueued_wall,
        latest_enqueued_wall: work.latest_enqueued_wall,
        dirty_all: false,
        paths: BTreeSet::new(),
        git_author_name: work.git_author_name.clone(),
        git_author_email: work.git_author_email.clone(),
        message_first_lines: VecDeque::new(),
        message_total: 0,
    });

    repo.pending_requests = repo.pending_requests.saturating_add(work.pending_requests);
    repo.message_total = repo.message_total.saturating_add(work.message_total);
    if work.earliest_enqueued_at < repo.earliest_enqueued_at {
        repo.earliest_enqueued_at = work.earliest_enqueued_at;
    }
    if work.earliest_enqueued_wall < repo.earliest_enqueued_wall {
        repo.earliest_enqueued_wall = work.earliest_enqueued_wall;
    }
    if work.latest_enqueued_wall > repo.latest_enqueued_wall {
        repo.latest_enqueued_wall = work.latest_enqueued_wall;
    }

    repo.git_author_name = work.git_author_name;
    repo.git_author_email = work.git_author_email;

    for line in work.message_first_lines {
        if repo.message_first_lines.len() >= COALESCER_SPILL_MESSAGE_CAP {
            break;
        }
        repo.message_first_lines.push_back(line);
    }

    if repo.dirty_all || work.dirty_all {
        repo.dirty_all = true;
        repo.paths.clear();
    } else {
        for path in work.paths {
            repo.paths.insert(path);
            if repo.paths.len() > COALESCER_SPILL_PATH_CAP {
                repo.dirty_all = true;
                repo.paths.clear();
                break;
            }
        }
    }

    rq.depth.fetch_add(work.pending_requests, Ordering::Relaxed);
}

/// Recompute and store the true per-repo queue depth.
fn coalescer_reconcile_repo_depth(rq: &RepoQueue) -> u64 {
    let queue_depth = {
        let queue = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
        u64::try_from(queue.len()).unwrap_or(u64::MAX)
    };
    let spill_depth = {
        let spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        spill.inner.as_ref().map_or(0, |repo| repo.pending_requests)
    };
    let actual = queue_depth.saturating_add(spill_depth);
    rq.depth.store(actual, Ordering::Relaxed);
    actual
}

fn format_coalescer_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn coalescer_batch_commit_message(requests: &[CoalescerCommitFields]) -> String {
    let Some(first) = requests.first() else {
        return "batch: 0 events".to_string();
    };

    let mut ts_first = first.enqueued_wall;
    let mut ts_last = first.enqueued_wall;
    for request in &requests[1..] {
        if request.enqueued_wall < ts_first {
            ts_first = request.enqueued_wall;
        }
        if request.enqueued_wall > ts_last {
            ts_last = request.enqueued_wall;
        }
    }

    format!(
        "batch: {} events ({} – {})",
        requests.len(),
        format_coalescer_ts(ts_first),
        format_coalescer_ts(ts_last),
    )
}

fn coalescer_commit_spilled_work(
    work: &CoalescerSpilledWork,
    stats: &Arc<Mutex<CommitQueueStats>>,
    batch_sizes: &Arc<Mutex<VecDeque<usize>>>,
) -> CoalescerSpillOutcome {
    if work.pending_requests == 0 {
        return CoalescerSpillOutcome {
            committed_requests: 0,
            committed_commits: 0,
            failed_work: None,
        };
    }

    let config = Config {
        git_author_name: work.git_author_name.clone(),
        git_author_email: work.git_author_email.clone(),
        ..Config::default()
    };

    let mut msg = format!(
        "batch: {} events ({} – {})",
        work.pending_requests,
        format_coalescer_ts(work.earliest_enqueued_wall),
        format_coalescer_ts(work.latest_enqueued_wall),
    );
    if work.dirty_all {
        msg.push_str("\n\ncommit-all spill overflow\n");
    } else {
        msg.push_str("\n\n");
    }

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
            .map(|()| (work.pending_requests, 1))
    } else if work.paths.is_empty() {
        Ok((work.pending_requests, 0))
    } else {
        coalescer_commit_with_retry(&work.repo_root, &config, &msg, &work.paths)
            .map(|()| (work.pending_requests, 1))
    };

    // Update stats
    match commit_result {
        Ok((batch_u64, commit_count)) => {
            let batch_size = usize::try_from(batch_u64).unwrap_or(usize::MAX);
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if commit_count > 0 {
                s.commits += usize::try_from(commit_count).unwrap_or(usize::MAX);
            }
            s.batched += batch_size;

            let mut sizes = batch_sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if commit_count > 0 {
                record_batch_size_samples(
                    &mut s,
                    &mut sizes,
                    batch_size,
                    usize::try_from(commit_count).unwrap_or(usize::MAX),
                );
            }
            CoalescerSpillOutcome {
                committed_requests: batch_u64,
                committed_commits: commit_count,
                failed_work: None,
            }
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
            CoalescerSpillOutcome {
                committed_requests: 0,
                committed_commits: 0,
                failed_work: Some(CoalescerSpilledWork {
                    repo_root: work.repo_root.clone(),
                    pending_requests: work.pending_requests,
                    earliest_enqueued_at: work.earliest_enqueued_at,
                    earliest_enqueued_wall: work.earliest_enqueued_wall,
                    latest_enqueued_wall: work.latest_enqueued_wall,
                    dirty_all: work.dirty_all,
                    paths: work.paths.clone(),
                    git_author_name: work.git_author_name.clone(),
                    git_author_email: work.git_author_email.clone(),
                    message_first_lines: work.message_first_lines.clone(),
                    message_total: work.message_total,
                }),
            }
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
) -> CoalescerCommitOutcome {
    if requests.is_empty() {
        return CoalescerCommitOutcome {
            committed_requests: 0,
            committed_commits: 0,
            failed_requests: Vec::new(),
        };
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
    let commit_result = if can_merge && requests.len() <= configured_coalescer_batch_size() {
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

        let mut combined_msg = coalescer_batch_commit_message(requests);
        if !summary_lines.is_empty() {
            combined_msg.push_str("\n\n");
            combined_msg.push_str(&summary_lines.join("\n"));
        }

        coalescer_commit_with_retry(repo_root, &config, &combined_msg, &merged_paths)
            .map(|()| (1, requests.len()))
    } else if requests.len() == 1 {
        // Single request — commit directly
        coalescer_commit_with_retry(
            repo_root,
            &config,
            &requests[0].message,
            &requests[0].rel_paths,
        )
        .map(|()| (1, 1))
    } else {
        // Path conflicts — commit sequentially
        let mut total = 0;
        let mut failed_requests = Vec::new();
        for req in requests {
            let r_config = Config {
                git_author_name: req.git_author_name.clone(),
                git_author_email: req.git_author_email.clone(),
                ..Config::default()
            };
            match coalescer_commit_with_retry(repo_root, &r_config, &req.message, &req.rel_paths) {
                Ok(()) => total += 1,
                Err(e) => {
                    tracing::warn!(
                        "[commit-coalescer] sequential request commit failed: paths={} err={e}",
                        req.rel_paths.len()
                    );
                    failed_requests.push(req.clone());
                }
            }
        }
        if failed_requests.is_empty() {
            Ok((total, total))
        } else {
            let committed = requests.len().saturating_sub(failed_requests.len());
            tracing::warn!(
                "[commit-coalescer] partial failure in sequential batch: committed={committed} failed={} total={}",
                failed_requests.len(),
                requests.len()
            );
            mcp_agent_mail_core::global_metrics()
                .storage
                .commit_errors_total
                .add(u64::try_from(failed_requests.len()).unwrap_or(u64::MAX));
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.commits += total;
            s.batched += total;
            s.errors += failed_requests.len();

            let mut sizes = batch_sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            record_batch_size_samples(&mut s, &mut sizes, 1, total);

            return CoalescerCommitOutcome {
                committed_requests: u64::try_from(committed).unwrap_or(u64::MAX),
                committed_commits: u64::try_from(committed).unwrap_or(u64::MAX),
                failed_requests,
            };
        }
    };

    // Update stats
    match commit_result {
        Ok((num_commits, batched_items)) => {
            let mut s = stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.commits += num_commits;
            s.batched += batched_items;

            let mut sizes = batch_sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if num_commits > 0 {
                record_batch_size_samples(
                    &mut s,
                    &mut sizes,
                    batched_items / num_commits,
                    num_commits,
                );
            }
            CoalescerCommitOutcome {
                committed_requests: u64::try_from(batched_items).unwrap_or(u64::MAX),
                committed_commits: u64::try_from(num_commits).unwrap_or(u64::MAX),
                failed_requests: Vec::new(),
            }
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
            CoalescerCommitOutcome {
                committed_requests: 0,
                committed_commits: 0,
                failed_requests: requests.to_vec(),
            }
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

        let commit_start = std::time::Instant::now();
        match run_with_lock_owner(repo_root, || commit_paths(&repo, config, message, &refs)) {
            Ok(()) => {
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
                        let start2 = std::time::Instant::now();
                        let result = run_with_lock_owner(repo_root, || {
                            commit_paths(&repo2, config, message, &refs2)
                        });
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

        let commit_start = std::time::Instant::now();
        match run_with_lock_owner(repo_root, || commit_all(&repo, config, message)) {
            Ok(()) => {
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
                        let start2 = std::time::Instant::now();
                        let result =
                            run_with_lock_owner(repo_root, || commit_all(&repo2, config, message));
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
    COMMIT_COALESCER.get_or_init(|| CommitCoalescer::new(configured_coalescer_flush_interval()))
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

    let mut common_project: Option<String> = None;

    for path in rel_paths {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 && parts[0] == "projects" {
            let project_slug = parts[1];
            if let Some(ref current) = common_project {
                if current != project_slug {
                    return repo_root.join(".commit.lock");
                }
            } else {
                common_project = Some(project_slug.to_string());
            }
        } else {
            return repo_root.join(".commit.lock");
        }
    }

    if let Some(slug) = common_project {
        repo_root.join("projects").join(slug).join(".commit.lock")
    } else {
        repo_root.join(".commit.lock")
    }
}

#[cfg(fuzzing)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzWbqEnqueueOutcome {
    pub result: WbqEnqueueResult,
    pub depth: u64,
    pub remaining_messages: usize,
}

#[cfg(fuzzing)]
pub fn fuzz_wbq_enqueue_scenario(
    capacity: u8,
    prefill: u8,
    drop_receiver: bool,
    disk_pressure_level: u64,
) -> FuzzWbqEnqueueOutcome {
    let capacity = usize::from(capacity.clamp(1, 8));
    let prefill = usize::from(prefill).min(capacity.saturating_sub(1));
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);

    for index in 0..prefill {
        let _ = tx.try_send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("fuzz-prefill-{index}"),
                agent_name: "FuzzAgent".to_string(),
            }),
        }));
    }

    let op_depth = AtomicU64::new(0);
    if drop_receiver {
        drop(rx);
        let result = wbq_enqueue_with_sender_and_pressure(
            &tx,
            &op_depth,
            WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: "fuzz-disconnected".to_string(),
                agent_name: "FuzzAgent".to_string(),
            },
            disk_pressure_level,
        );
        return FuzzWbqEnqueueOutcome {
            result,
            depth: op_depth.load(Ordering::Relaxed),
            remaining_messages: 0,
        };
    }

    let result = wbq_enqueue_with_sender_and_pressure(
        &tx,
        &op_depth,
        WriteOp::ClearSignal {
            config: Config::default(),
            project_slug: "fuzz-live".to_string(),
            agent_name: "FuzzAgent".to_string(),
        },
        disk_pressure_level,
    );
    drop(tx);
    let remaining_messages = rx.try_iter().count();

    FuzzWbqEnqueueOutcome {
        result,
        depth: op_depth.load(Ordering::Relaxed),
        remaining_messages,
    }
}

#[cfg(fuzzing)]
pub fn fuzz_coalescer_batch_summary(wall_offsets_seconds: &[i64]) -> String {
    let base = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap_or_else(Utc::now);
    let requests: Vec<CoalescerCommitFields> = wall_offsets_seconds
        .iter()
        .take(128)
        .enumerate()
        .map(|(index, offset)| CoalescerCommitFields {
            enqueued_at: Instant::now(),
            enqueued_wall: base + chrono::Duration::seconds(offset.rem_euclid(86_400)),
            git_author_name: "Fuzz".to_string(),
            git_author_email: "fuzz@example.invalid".to_string(),
            message: format!("fuzz commit {index}"),
            rel_paths: vec![format!("projects/fuzz/file-{index}.json")],
        })
        .collect();

    coalescer_batch_commit_message(&requests)
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

fn git_index_lock_paths_checked(repo_root: &Path) -> Option<(PathBuf, PathBuf)> {
    let git_dir = repo_root.join(".git");
    let lock_path = git_dir.join("index.lock");
    let owner_path = git_dir.join("index.lock.owner");

    for (label, path) in [
        ("git index lock path", lock_path.as_path()),
        ("git index lock owner path", owner_path.as_path()),
    ] {
        if path_existing_prefix_has_symlink(path).unwrap_or(true) {
            tracing::warn!(
                "[git-lock] refusing to use {label} through symlinked path: {}",
                path.display()
            );
            return None;
        }
    }

    Some((lock_path, owner_path))
}

/// Write a `.git/index.lock.owner` file with our PID and timestamp.
///
/// This allows other processes to check if the lock holder is still alive
/// before forcibly removing a lock.
fn write_lock_owner(repo_root: &Path) {
    let Some((_lock_path, owner_path)) = git_index_lock_paths_checked(repo_root) else {
        return;
    };
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let start_ticks = process_start_ticks(pid).unwrap_or(0);
    let content = format!("{pid}\n{ts}\n{start_ticks}\n");
    let _ = fs::write(&owner_path, content);
}

/// Remove the `.git/index.lock.owner` file after a successful commit.
fn remove_lock_owner(repo_root: &Path) {
    let Some((_lock_path, owner_path)) = git_index_lock_paths_checked(repo_root) else {
        return;
    };
    let _ = fs::remove_file(&owner_path);
}

/// Run a commit attempt while ensuring the owner sidecar is cleaned up.
///
/// The owner file is advisory metadata for a single attempt and should never
/// survive an attempt outcome.
fn run_with_lock_owner<T>(repo_root: &Path, op: impl FnOnce() -> Result<T>) -> Result<T> {
    write_lock_owner(repo_root);
    let result = op();
    remove_lock_owner(repo_root);
    result
}

/// Check if a process with the given PID is still alive.
///
/// Prefers `/proc/<pid>` when available (Linux semantics), and falls back to
/// `pid_alive` for platforms without `/proc`.
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if Path::new("/proc").exists() {
            return Path::new(&format!("/proc/{pid}")).exists();
        }
    }
    pid_alive(pid)
}

#[cfg(target_os = "linux")]
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close_paren = stat.rfind(')')?;
    let rest = stat.get(close_paren + 2..)?;
    // Fields after `comm` begin at field 3 (`state`), so starttime (field 22)
    // is offset 19 in this tail segment.
    rest.split_whitespace().nth(19)?.parse::<u64>().ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start_ticks(_pid: u32) -> Option<u64> {
    None
}

fn lock_file_age_seconds(lock_path: &Path) -> Option<f64> {
    fs::metadata(lock_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs_f64())
}

/// Try to clean up a stale .git/index.lock file with PID-aware safety.
///
/// Before removing a lock:
/// 1. Check if a `.git/index.lock.owner` file exists with the owner PID
/// 2. If the owning PID is still alive, do NOT remove the lock
/// 3. If the PID is dead (or no owner file), use age-based threshold
///
/// Returns `true` if a stale lock was removed.
pub fn clean_stale_git_index_lock(repo_root: &Path, max_age_seconds: f64) -> bool {
    try_clean_stale_git_lock(repo_root, max_age_seconds)
}

/// Try to clean up a stale `.git/index.lock` file with PID-aware safety.
///
/// See [`clean_stale_git_index_lock`] for the supported cleanup semantics.
fn try_clean_stale_git_lock(repo_root: &Path, max_age_seconds: f64) -> bool {
    let Some((lock_path, owner_path)) = git_index_lock_paths_checked(repo_root) else {
        return false;
    };
    if !path_is_nonsymlink_file(&lock_path) {
        return false;
    }

    // Check PID-based ownership first
    if path_is_nonsymlink_file(&owner_path)
        && let Ok(content) = fs::read_to_string(&owner_path)
    {
        let lines: Vec<&str> = content.lines().collect();
        if let Some(pid_str) = lines.first()
            && let Ok(pid) = pid_str.trim().parse::<u32>()
        {
            let has_start_ticks_field = lines.get(2).is_some();
            let owner_start_ticks = lines
                .get(2)
                .and_then(|line| line.trim().parse::<u64>().ok())
                .filter(|ticks| *ticks > 0);
            if is_pid_alive(pid) {
                // If PID was recycled, the owner start-ticks will mismatch.
                // That means the lock is stale even though the PID exists now.
                if let Some(expected_ticks) = owner_start_ticks
                    && let Some(actual_ticks) = process_start_ticks(pid)
                    && actual_ticks != expected_ticks
                {
                    tracing::info!(
                        "[git-lock] index.lock owner PID {pid} reused (start ticks mismatch), removing stale lock"
                    );
                    let _ = fs::remove_file(&owner_path);
                    let _ = fs::remove_file(&lock_path);
                    return true;
                }

                // New-format owner files may carry "unknown" start ticks (e.g., non-Linux).
                // Be conservative: never remove an alive-PID lock in that case.
                if has_start_ticks_field {
                    // If we cannot verify start ticks (e.g., macOS, Windows, or reading a 0),
                    // we must fall back to an age-based timeout to prevent permanent deadlocks
                    // from PID reuse. We use a more conservative timeout (2x) than the legacy format.
                    if (owner_start_ticks.is_none() || process_start_ticks(pid).is_none())
                        && lock_file_age_seconds(&lock_path)
                            .is_some_and(|age| age > max_age_seconds * 2.0)
                    {
                        tracing::info!(
                            "[git-lock] index.lock held by alive PID {pid} but start ticks unavailable, force clean (age > {:.1}s)",
                            max_age_seconds * 2.0
                        );
                        let _ = fs::remove_file(&owner_path);
                        let _ = fs::remove_file(&lock_path);
                        return true;
                    }

                    tracing::debug!(
                        "[git-lock] index.lock held by alive PID {pid} (start ticks unavailable), not removing"
                    );
                    return false;
                }

                // Legacy owner files (no start-tick field) can stick forever on PID reuse.
                // If the lock is sufficiently old, treat as stale.
                if lock_file_age_seconds(&lock_path).is_some_and(|age| age > max_age_seconds) {
                    tracing::info!(
                        "[git-lock] legacy owner format with alive PID {pid} and stale age, removing lock"
                    );
                    let _ = fs::remove_file(&owner_path);
                    let _ = fs::remove_file(&lock_path);
                    return true;
                }

                tracing::debug!("[git-lock] index.lock held by alive PID {pid}, not removing");
                return false;
            }
            // PID is dead — safe to remove lock.
            // Remove owner before lock so a racing cleaner cannot pair
            // stale owner metadata with a freshly acquired lock.
            tracing::info!("[git-lock] index.lock held by dead PID {pid}, removing stale lock");
            let _ = fs::remove_file(&owner_path);
            let _ = fs::remove_file(&lock_path);
            return true;
        }
    }

    // No owner file or unparseable — fall back to age-based removal
    let age = lock_file_age_seconds(&lock_path);

    if let Some(age) = age
        && age > max_age_seconds
    {
        tracing::info!("[git-lock] removing stale index.lock (age={age:.1}s > {max_age_seconds}s)");
        let _ = fs::remove_file(&owner_path);
        let _ = fs::remove_file(&lock_path);
        return true;
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
    let mut updates: Vec<(String, Option<git2::Oid>)> = Vec::new();
    for path in rel_paths {
        let path = validate_repo_relative_path("lockfree commit path", path)?;
        let full = workdir.join(path);
        match repo.blob_path(&full) {
            Ok(blob_oid) => updates.push((path.to_string(), Some(blob_oid))),
            Err(err) if err.code() == git2::ErrorCode::NotFound => {
                // Missing files are deletions/moves from the archive's point of view.
                updates.push((path.to_string(), None));
            }
            Err(err) => return Err(err.into()),
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
        tracing::warn!("[git-lockfree] failed to sync index after commit: {err}");
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
    updates: &[(String, Option<git2::Oid>)],
) -> Result<git2::Oid> {
    // Group updates by first path component
    let mut direct_entries: Vec<(&str, Option<git2::Oid>)> = Vec::new();
    let mut by_prefix: HashMap<String, Vec<(String, Option<git2::Oid>)>> = HashMap::new();

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
    for (name, oid_opt) in &direct_entries {
        if let Some(oid) = oid_opt {
            builder.insert(name, *oid, 0o100_644)?;
        } else {
            let _ = builder.remove(name);
        }
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
        let commit_start = std::time::Instant::now();
        match run_with_lock_owner(repo_root, || {
            commit_paths(&repo, config, message, rel_paths)
        }) {
            Ok(()) => {
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
    let mut metadata_candidates: Vec<PathBuf> = Vec::new();

    fn should_force_deep_scan() -> bool {
        std::env::var("AM_HEAL_LOCKS_DEEP_SCAN").is_ok_and(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }

    fn maybe_collect_metadata(path: &Path, metadata_candidates: &mut Vec<PathBuf>) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".lock.owner.json") {
            metadata_candidates.push(path.to_path_buf());
        }
    }

    fn maybe_cleanup_lock(path: &Path, result: &mut HealResult) {
        if path.extension().is_none_or(|e| e != "lock") {
            return;
        }
        // Never remove Git's index lock via generic flock heuristics.
        // Git index.lock is existence-based and can be active even when no
        // advisory file lock is held.
        if path
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("index.lock"))
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == std::ffi::OsStr::new(".git"))
        {
            return;
        }

        result.locks_scanned += 1;

        // Try to acquire exclusive lock. If successful, it means no one else
        // is holding it, so we can treat it as a stale artifact from a previous run.
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
            use fs2::FileExt;
            if f.try_lock_exclusive().is_ok() {
                let mut removed = false;
                match std::fs::remove_file(path) {
                    Ok(()) => removed = true,
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        // Windows can require closing/unlocking first before unlinking.
                        let _ = f.unlock();
                        drop(f);
                        removed = std::fs::remove_file(path).is_ok();
                    }
                    Err(_) => {}
                }

                if removed {
                    result.locks_removed.push(path.display().to_string());
                    // Try to remove corresponding metadata file.
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let meta_path = path.with_file_name(format!("{name}.owner.json"));
                    let _ = std::fs::remove_file(meta_path);
                }
            }
        }
    }

    fn walk_recursive(
        dir: &Path,
        result: &mut HealResult,
        metadata_candidates: &mut Vec<PathBuf>,
    ) -> std::io::Result<()> {
        if path_existing_prefix_has_symlink(dir)? || !path_is_nonsymlink_dir(dir) {
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
                walk_recursive(&path, result, metadata_candidates)?;
            } else if file_type.is_file() {
                maybe_cleanup_lock(&path, result);
                maybe_collect_metadata(&path, metadata_candidates);
            }
        }
        Ok(())
    }

    fn scan_dir_shallow(
        dir: &Path,
        result: &mut HealResult,
        metadata_candidates: &mut Vec<PathBuf>,
    ) -> std::io::Result<()> {
        if path_existing_prefix_has_symlink(dir)? || !path_is_nonsymlink_dir(dir) {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            maybe_cleanup_lock(&path, result);
            maybe_collect_metadata(&path, metadata_candidates);
        }
        Ok(())
    }

    fn gather_lock_scan_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        let mut push_dir = |dir: PathBuf| {
            if path_is_nonsymlink_dir(&dir) && seen.insert(dir.clone()) {
                dirs.push(dir);
            }
        };

        push_dir(root.to_path_buf());
        push_dir(root.join(".git"));

        let projects_dir = root.join("projects");
        push_dir(projects_dir.clone());
        if path_is_nonsymlink_dir(&projects_dir) {
            for entry in fs::read_dir(&projects_dir)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() && !file_type.is_symlink() {
                    push_dir(entry.path());
                }
            }
        }

        Ok(dirs)
    }

    if should_force_deep_scan() {
        walk_recursive(root, &mut result, &mut metadata_candidates)?;
    } else {
        for dir in gather_lock_scan_dirs(root)? {
            scan_dir_shallow(&dir, &mut result, &mut metadata_candidates)?;
        }
    }

    // Clean orphaned metadata files (no matching lock) without re-walking.
    for path in metadata_candidates {
        if !path.exists() {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some(lock_name) = name.strip_suffix(".owner.json") else {
            continue;
        };
        let lock_candidate = parent.join(lock_name);
        if !lock_candidate.exists() && fs::remove_file(&path).is_ok() {
            result.metadata_removed.push(path.display().to_string());
        }
    }

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

#[cfg(test)]
fn repo_cache_clear_for_test() {
    let mut guard = REPO_CACHE.lock();
    if let Some(map) = guard.as_mut() {
        map.clear();
    }
}

// ---------------------------------------------------------------------------
// Directory existence cache — avoids repeated stat() syscalls from
// create_dir_all() on directories that already exist.
// ---------------------------------------------------------------------------

static DIR_CACHE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Clone, Debug)]
struct CanonicalPathCacheEntry {
    canonical: PathBuf,
    validated_at: Instant,
}

const CANONICAL_PATH_CACHE_MAX_ENTRIES: usize = 2048;
#[cfg(test)]
const CANONICAL_PATH_CACHE_FRESHNESS: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const CANONICAL_PATH_CACHE_FRESHNESS: Duration = Duration::from_secs(2);

static CANONICAL_PATH_CACHE: LazyLock<Mutex<HashMap<PathBuf, CanonicalPathCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
    if path_existing_prefix_has_symlink(dir)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to create directory through symlinked path: {}",
                dir.display()
            ),
        ));
    }
    fs::create_dir_all(dir)?;
    {
        let mut cache = DIR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(dir.to_path_buf());
    }
    Ok(())
}

/// Canonicalize an absolute path with a short-lived process-local cache.
///
/// This targets hot repeated root/base-directory lookups while preserving the
/// original syscall-backed behavior for relative and one-off paths. Only paths
/// that are already canonical are cached, so symlinked or otherwise rewritten
/// inputs still revalidate through the filesystem each time.
fn canonicalize_path_cached(path: &Path) -> std::io::Result<PathBuf> {
    if !path.is_absolute() {
        return path.canonicalize();
    }

    {
        let mut cache = CANONICAL_PATH_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cache.get(path).cloned() {
            if entry.validated_at.elapsed() <= CANONICAL_PATH_CACHE_FRESHNESS {
                return Ok(entry.canonical);
            }
            cache.remove(path);
        }
    }

    let canonical = path.canonicalize()?;
    if canonical != path {
        return Ok(canonical);
    }

    let mut cache = CANONICAL_PATH_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !cache.contains_key(path)
        && cache.len() >= CANONICAL_PATH_CACHE_MAX_ENTRIES
        && let Some(victim) = cache.keys().next().cloned()
    {
        cache.remove(&victim);
    }
    cache.insert(
        path.to_path_buf(),
        CanonicalPathCacheEntry {
            canonical: canonical.clone(),
            validated_at: Instant::now(),
        },
    );
    Ok(canonical)
}

fn normalize_repo_root_key(repo_root: &Path) -> PathBuf {
    canonicalize_path_cached(repo_root).unwrap_or_else(|_| repo_root.to_path_buf())
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

/// Open an existing per-project archive directory without creating it.
pub fn open_archive(config: &Config, slug: &str) -> Result<Option<ProjectArchive>> {
    if slug.contains('/')
        || slug.contains('\\')
        || slug.contains("..")
        || slug.is_empty()
        || slug == "."
        || slug.starts_with('.')
    {
        return Err(StorageError::InvalidPath(
            "invalid project slug: must not contain path separators or '..' components".to_string(),
        ));
    }

    let repo_root = config.storage_root.clone();
    let project_root = repo_root.join("projects").join(slug);
    if path_existing_prefix_has_symlink(&project_root)? {
        return Ok(None);
    }
    if !fs::symlink_metadata(&project_root).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Ok(None);
    }

    let canonical_repo_root =
        canonicalize_path_cached(&repo_root).unwrap_or_else(|_| repo_root.clone());

    Ok(Some(ProjectArchive {
        slug: slug.to_string(),
        root: project_root.clone(),
        repo_root,
        lock_path: project_root.join(".archive.lock"),
        canonical_repo_root,
    }))
}

/// Ensure a per-project archive directory exists under the archive root.
pub fn ensure_archive(config: &Config, slug: &str) -> Result<ProjectArchive> {
    // Reject slugs with path separators or traversal components.
    if slug.contains('/')
        || slug.contains('\\')
        || slug.contains("..")
        || slug.is_empty()
        || slug == "."
        || slug.starts_with('.')
    {
        return Err(StorageError::InvalidPath(
            "invalid project slug: must not contain path separators or '..' components".to_string(),
        ));
    }
    let (repo_root, _fresh) = ensure_archive_root(config)?;
    let project_root = repo_root.join("projects").join(slug);
    ensure_dir(&project_root)?;

    let canonical_repo_root =
        canonicalize_path_cached(&repo_root).unwrap_or_else(|_| repo_root.clone());
    Ok(ProjectArchive {
        slug: slug.to_string(),
        root: project_root.clone(),
        repo_root,
        lock_path: project_root.join(".archive.lock"),
        canonical_repo_root,
    })
}

/// Persist stable project metadata used by DB reconstruction.
///
/// Writes `projects/{slug}/project.json` containing the canonical `human_key`.
/// If the file already exists with identical content, this is a no-op.
pub fn write_project_metadata_with_config(
    archive: &ProjectArchive,
    config: &Config,
    human_key: &str,
) -> Result<()> {
    if !Path::new(human_key).is_absolute() {
        return Err(StorageError::InvalidPath(
            "project human_key must be an absolute path".to_string(),
        ));
    }

    let repo_root = archive_repo_root_checked(archive)?;
    let project_root = archive_project_root_checked(archive)?;
    let metadata_path = project_root.join("project.json");
    let metadata = serde_json::json!({
        "slug": archive.slug,
        "human_key": human_key,
    });

    // Avoid needless commits when ensure_project is called repeatedly.
    if let Ok(existing) = fs::read_to_string(&metadata_path)
        && let Ok(existing_json) = serde_json::from_str::<serde_json::Value>(&existing)
        && existing_json == metadata
    {
        return Ok(());
    }

    write_json(&metadata_path, &metadata, true)?;
    let rel = rel_path_cached(&archive.canonical_repo_root, &metadata_path)?;
    enqueue_async_commit(
        repo_root,
        config,
        &format!("project: metadata {}", archive.slug),
        &[rel],
    );
    Ok(())
}

/// Configure `git gc` / `maintenance` / `repack` defaults on the archive repo
/// so long-running servers don't accumulate an unbounded loose-object pile.
///
/// Idempotent. Only sets keys that aren't already configured, so an operator
/// who has tuned their archive by hand is never overridden.
///
/// Why this matters: every mail event does a `git add + commit`, and git's
/// stock `gc.auto=6700` means a busy mailbox can sit on hundreds of thousands
/// of loose objects for days before auto-gc kicks in. Lowering `gc.auto` to
/// 256 makes auto-repack fire during normal commit paths. `gc.pruneExpire=now`
/// lets unreferenced objects be reaped immediately (safe for a forward-only
/// archive that never rewrites history).
fn configure_archive_git_defaults(repo: &Repository) {
    let Ok(mut full_cfg) = repo.config() else {
        tracing::debug!("archive gc config skipped: repo.config() unavailable");
        return;
    };
    // Read local-only view so a user's global ~/.gitconfig doesn't hide our
    // archive-local tuning. Write-back still happens through `full_cfg`,
    // which persists into the repo's local config file.
    let local_cfg = full_cfg.open_level(git2::ConfigLevel::Local).ok();

    let key_is_unset_locally =
        |key: &str| -> bool { local_cfg.as_ref().is_none_or(|c| c.get_entry(key).is_err()) };

    if key_is_unset_locally("gc.auto") {
        let _ = full_cfg.set_i64("gc.auto", 256);
    }
    if key_is_unset_locally("gc.autoPackLimit") {
        let _ = full_cfg.set_i64("gc.autoPackLimit", 4);
    }
    if key_is_unset_locally("gc.pruneExpire") {
        let _ = full_cfg.set_str("gc.pruneExpire", "now");
    }
    if key_is_unset_locally("maintenance.auto") {
        let _ = full_cfg.set_bool("maintenance.auto", true);
    }
    if key_is_unset_locally("repack.writeBitmaps") {
        let _ = full_cfg.set_bool("repack.writeBitmaps", true);
    }
}

/// Initialize a git repository at `root` if one does not already exist.
///
/// Configures gpgsign=false, archive-specific gc defaults, and writes
/// `.gitattributes`. For pre-existing archives, applies the gc defaults once
/// per process so older installs don't keep accumulating loose objects.
///
/// Returns `true` if a new repo was created, `false` if it already existed.
fn ensure_repo(root: &Path, config: &Config) -> Result<bool> {
    if repo_cache_contains(root) {
        return Ok(false);
    }

    let git_dir = root.join(".git");
    if git_dir.exists() {
        // Pre-existing archive: apply gc defaults once. `configure_archive_git_defaults`
        // is idempotent and respects operator-set values, so this is safe to run
        // every time a cold process first opens the archive.
        if let Ok(existing) = Repository::open(root) {
            configure_archive_git_defaults(&existing);
        }
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

    // Apply archive-specific gc/maintenance tuning.
    configure_archive_git_defaults(&repo);

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
            true,
        )?;
    }

    // Initial commit (retry-enabled for consistency with other archive writes)
    commit_paths_with_retry(
        root,
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
    write_agent_profile_with_config(archive, &Config::get(), agent)
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

    let repo_root = archive_repo_root_checked(archive)?;
    let project_root = archive_project_root_checked(archive)?;
    let profile_dir = project_root.join("agents").join(name);
    ensure_dir(&profile_dir)?;

    let profile_path = profile_dir.join("profile.json");
    write_json(&profile_path, agent, true)?;

    let rel = rel_path_cached(&archive.canonical_repo_root, &profile_path)?;
    enqueue_async_commit(repo_root, config, &format!("agent: profile {name}"), &[rel]);

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

    let repo_root = archive_repo_root_checked(archive)?;
    let mut rel_paths = Vec::new();
    let mut entries = Vec::new();
    let mut normalized_reservations = Vec::with_capacity(reservations.len());
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

        let agent_name = normalized
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        normalized_reservations.push((normalized, path_pattern, agent_name));
    }

    let project_root = archive_project_root_checked(archive)?;
    let reservation_dir = project_root.join("file_reservations");
    ensure_dir(&reservation_dir)?;

    for (normalized, path_pattern, agent_name) in normalized_reservations {
        // Legacy path: sha1(path_pattern).json
        let digest = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(path_pattern.as_bytes());
            hex::encode(hasher.finalize())
        };
        let legacy_path = reservation_dir.join(format!("{digest}.json"));
        write_json(&legacy_path, &normalized, true)?;
        rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &legacy_path)?);

        // Stable per-reservation artifact: id-<id>.json
        if let Some(id) = normalized.get("id").and_then(serde_json::Value::as_i64) {
            let id_path = reservation_dir.join(format!("id-{id}.json"));
            write_json(&id_path, &normalized, true)?;
            rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &id_path)?);
        }

        entries.push((agent_name, path_pattern));
    }

    let commit_msg = build_file_reservation_commit_message(&entries);
    enqueue_async_commit(repo_root, config, &commit_msg, &rel_paths);

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
    RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9._-]+").unwrap_or_else(|_| unreachable!()))
}

fn slugify_message_subject(subject: &str) -> String {
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
    for c in p.components() {
        match c {
            Component::Normal(part) => {
                if part == ".git" {
                    return Err(StorageError::InvalidPath(format!(
                        "{kind} must not reference .git internals"
                    )));
                }
            }
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
    let project_root = archive_project_root_checked(archive)?;

    let y = created.format("%Y").to_string();
    let m = created.format("%m").to_string();
    let iso = created.format("%Y-%m-%dT%H-%M-%SZ").to_string();

    let slug = slugify_message_subject(subject);

    let filename = if id > 0 {
        format!("{iso}__{slug}__{id}.md")
    } else {
        format!("{iso}__{slug}.md")
    };

    let canonical = project_root
        .join("messages")
        .join(&y)
        .join(&m)
        .join(&filename);
    let outbox = project_root
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
            project_root
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

fn render_message_bundle_content(message: &serde_json::Value, body_md: &str) -> Result<String> {
    let frontmatter = serde_json::to_string_pretty(message)?;
    Ok(format!("---json\n{frontmatter}\n---\n\n{body_md}"))
}

fn redact_message_bcc_for_inbox(message: &serde_json::Value) -> serde_json::Value {
    let mut redacted = message.clone();
    if let Some(obj) = redacted.as_object_mut()
        && obj
            .get("bcc")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|bcc| !bcc.is_empty())
    {
        obj.insert("bcc".to_string(), serde_json::Value::Array(vec![]));
    }
    redacted
}

struct MessageBundleCommitMeta {
    auto_commit_message: String,
    summary_line: String,
}

fn visible_recipient_label(visible_recipients: &[String]) -> String {
    if visible_recipients.is_empty() {
        "(hidden recipients)".to_string()
    } else {
        visible_recipients.join(", ")
    }
}

fn build_message_bundle_commit_meta(
    message: &serde_json::Value,
    sender: &str,
    visible_recipients: &[String],
    timestamp_str: &str,
) -> MessageBundleCommitMeta {
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

    let summary_line = format!(
        "mail: {sender} -> {} | {}",
        visible_recipient_label(visible_recipients),
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

    MessageBundleCommitMeta {
        auto_commit_message: format!("{summary_line}\n\n{}\n", body_lines.join("\n")),
        summary_line,
    }
}

fn append_message_bundle_files(
    archive: &ProjectArchive,
    message: &serde_json::Value,
    body_md: &str,
    sender: &str,
    recipients: &[String],
    extra_paths: &[String],
    rel_paths: &mut Vec<String>,
) -> Result<MessageBundleCommitMeta> {
    let created = parse_message_timestamp(message);
    let timestamp_str = created.to_rfc3339();
    let visible_recipients = message_visible_recipients(message);

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

    let full_content = render_message_bundle_content(message, body_md)?;
    let inbox_message = redact_message_bcc_for_inbox(message);
    let inbox_content = if inbox_message == *message {
        None
    } else {
        Some(render_message_bundle_content(&inbox_message, body_md)?)
    };
    let inbox_content_ref = inbox_content.as_deref().unwrap_or(&full_content);

    write_text(&paths.canonical, &full_content, true)?;
    rel_paths.push(rel_path_cached(
        &archive.canonical_repo_root,
        &paths.canonical,
    )?);

    write_text(&paths.outbox, &full_content, true)?;
    rel_paths.push(rel_path_cached(
        &archive.canonical_repo_root,
        &paths.outbox,
    )?);

    for inbox_path in &paths.inbox {
        write_text(inbox_path, inbox_content_ref, true)?;
        rel_paths.push(rel_path_cached(&archive.canonical_repo_root, inbox_path)?);
    }

    if let Some(thread_id) = message.get("thread_id").and_then(serde_json::Value::as_str) {
        let thread_id = thread_id.trim();
        if !thread_id.is_empty() {
            let canonical_rel = rel_path_cached(&archive.canonical_repo_root, &paths.canonical)?;
            if let Ok(digest_rel) = update_thread_digest(
                archive,
                thread_id,
                sender,
                &visible_recipients,
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

    for p in extra_paths {
        let p = validate_repo_relative_path("extra_path", p)?;
        rel_paths.push(p.to_string());
    }

    Ok(build_message_bundle_commit_meta(
        message,
        sender,
        &visible_recipients,
        &timestamp_str,
    ))
}

fn dedup_repo_relative_paths(rel_paths: &mut Vec<String>) {
    let mut seen = HashSet::with_capacity(rel_paths.len());
    rel_paths.retain(|path| seen.insert(path.clone()));
}

fn build_batch_message_bundle_commit_message(summary_lines: &[String]) -> String {
    const SUMMARY_LIMIT: usize = 32;

    let mut msg = format!("batch: {} message bundles\n\n", summary_lines.len());
    for line in summary_lines.iter().take(SUMMARY_LIMIT) {
        msg.push_str("- ");
        msg.push_str(line);
        msg.push('\n');
    }
    if summary_lines.len() > SUMMARY_LIMIT {
        msg.push_str(&format!(
            "- ... (+{} more)\n",
            summary_lines.len() - SUMMARY_LIMIT
        ));
    }
    msg
}

/// Write multiple message bundles to the archive and enqueue a single async commit.
pub fn write_message_batch_bundle(
    archive: &ProjectArchive,
    config: &Config,
    entries: &[MessageBundleBatchEntry<'_>],
    commit_text: Option<&str>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let repo_root = archive_repo_root_checked(archive)?;
    let estimated_rel_paths = entries
        .iter()
        .map(|entry| {
            2 + entry.recipients.len()
                + entry.extra_paths.len()
                + usize::from(
                    entry
                        .message
                        .get("thread_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|thread_id| !thread_id.trim().is_empty()),
                )
        })
        .sum();
    let mut rel_paths = Vec::with_capacity(estimated_rel_paths);
    let mut summary_lines = Vec::with_capacity(entries.len());
    let mut single_auto_commit_message: Option<String> = None;

    for entry in entries {
        let commit_meta = append_message_bundle_files(
            archive,
            entry.message,
            entry.body_md,
            entry.sender,
            entry.recipients,
            entry.extra_paths,
            &mut rel_paths,
        )?;
        if entries.len() == 1 {
            single_auto_commit_message = Some(commit_meta.auto_commit_message);
        }
        summary_lines.push(commit_meta.summary_line);
    }

    dedup_repo_relative_paths(&mut rel_paths);

    let commit_message = if let Some(text) = commit_text {
        text.to_string()
    } else if entries.len() == 1 {
        single_auto_commit_message.unwrap_or_default()
    } else {
        build_batch_message_bundle_commit_message(&summary_lines)
    };

    enqueue_async_commit(repo_root, config, &commit_message, &rel_paths);
    Ok(())
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
    let repo_root = archive_repo_root_checked(archive)?;
    let mut rel_paths = Vec::with_capacity(
        2 + recipients.len()
            + extra_paths.len()
            + usize::from(
                message
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|thread_id| !thread_id.trim().is_empty()),
            ),
    );
    let commit_meta = append_message_bundle_files(
        archive,
        message,
        body_md,
        sender,
        recipients,
        extra_paths,
        &mut rel_paths,
    )?;
    let commit_message = commit_text
        .map(ToString::to_string)
        .unwrap_or(commit_meta.auto_commit_message);

    enqueue_async_commit(repo_root, config, &commit_message, &rel_paths);

    Ok(())
}

fn message_visible_recipients(message: &serde_json::Value) -> Vec<String> {
    let mut visible = Vec::new();
    let mut seen = HashSet::new();

    for field in ["to", "cc"] {
        let Some(items) = message.get(field).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(name) = item.as_str().map(str::trim).filter(|name| !name.is_empty()) else {
                continue;
            };
            if seen.insert(name.to_ascii_lowercase()) {
                visible.push(name.to_string());
            }
        }
    }

    visible
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
    if let Some(serde_json::Value::Number(n)) = ts
        && let Some(raw) = n.as_i64()
        && raw > 0
    {
        let secs = raw / 1_000_000;
        let micros = raw % 1_000_000;
        if let Some(dt) = DateTime::from_timestamp(secs, (micros * 1000) as u32) {
            return dt;
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
    let project_root = archive_project_root_checked(archive)?;
    let digest_dir = project_root.join("messages").join("threads");
    ensure_dir(&digest_dir)?;

    let safe_thread_id = sanitize_thread_id(thread_id);
    let digest_path = digest_dir.join(format!("{safe_thread_id}.md"));
    let recipients_str = if recipients.is_empty() {
        "(hidden recipients)".to_string()
    } else {
        recipients.join(", ")
    };

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

/// Fallback attachment size limit used only when `Config::max_attachment_bytes`
/// is zero (unlimited).  In that case we still guard against pathological
/// decode times during WebP conversion.
const FALLBACK_MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

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

    if path_existing_prefix_has_symlink(webp_path)? || !path_is_nonsymlink_file(webp_path) {
        return Err(StorageError::InvalidPath(format!(
            "cached attachment WebP is missing or invalid: {}",
            webp_path.display()
        )));
    }
    if path_existing_prefix_has_symlink(manifest_path)? || !path_is_nonsymlink_file(manifest_path) {
        return Err(StorageError::InvalidPath(format!(
            "cached attachment manifest is missing or invalid: {}",
            manifest_path.display()
        )));
    }

    let manifest_str = fs::read_to_string(manifest_path)?;
    let manifest: AttachmentManifest = serde_json::from_str(&manifest_str)?;
    let project_root = archive_project_root_checked(archive)?;
    let repo_root = archive_repo_root_checked(archive)?;

    let webp_rel = rel_path_cached(&archive.canonical_repo_root, webp_path)?;
    let manifest_rel = rel_path_cached(&archive.canonical_repo_root, manifest_path)?;
    let webp_bytes_len = fs::metadata(webp_path)?.len() as usize;

    let original_rel = if config.keep_original_images {
        match manifest.original_path.as_deref() {
            Some(raw_rel) => {
                let raw_rel =
                    validate_repo_relative_path("cached attachment original path", raw_rel)?;
                let original_path = repo_root.join(raw_rel);
                if path_existing_prefix_has_symlink(&original_path)?
                    || !path_is_nonsymlink_file(&original_path)
                {
                    return Err(StorageError::InvalidPath(format!(
                        "cached attachment original is missing or invalid: {}",
                        original_path.display()
                    )));
                }
                Some(raw_rel.to_string())
            }
            None => None,
        }
    } else {
        None
    };

    let mut rel_paths = vec![webp_rel.clone(), manifest_rel];
    if let Some(ref original_rel) = original_rel {
        rel_paths.push(original_rel.clone());
    }

    // Cache artifacts may exist on disk even if a previous run crashed before
    // they were committed. Re-stage any deterministic sidecars that already
    // exist so a retry cannot publish a message without its cached attachment.
    let audit_path = project_root
        .join("attachments")
        .join("_audit")
        .join(format!("{digest}.log"));
    if !path_existing_prefix_has_symlink(&audit_path)? && path_is_nonsymlink_file(&audit_path) {
        rel_paths.push(rel_path_cached(&archive.canonical_repo_root, &audit_path)?);
    }

    let should_inline = match embed_policy {
        EmbedPolicy::Inline => true,
        EmbedPolicy::File => false,
        EmbedPolicy::Auto => webp_bytes_len <= config.inline_image_max_bytes,
    };

    let meta = if should_inline {
        let webp_bytes = fs::read(webp_path)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&webp_bytes);
        AttachmentMeta {
            kind: "inline".to_string(),
            media_type: "image/webp".to_string(),
            bytes: webp_bytes_len,
            sha1: digest.to_string(),
            width: manifest.width,
            height: manifest.height,
            data_base64: Some(encoded),
            path: None,
            original_path: original_rel.clone(),
        }
    } else {
        AttachmentMeta {
            kind: "file".to_string(),
            media_type: "image/webp".to_string(),
            bytes: webp_bytes_len,
            sha1: digest.to_string(),
            width: manifest.width,
            height: manifest.height,
            data_base64: None,
            path: Some(webp_rel),
            original_path: original_rel.clone(),
        }
    };

    Ok(StoredAttachment { meta, rel_paths })
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

    // Guard against pathological decode times during WebP conversion.
    // Use whichever is larger: the config limit (tool-layer) or the
    // conversion-safety fallback.  The tool layer already validates
    // against config.max_attachment_bytes; this guard is about OOM
    // prevention in the image decoder, so it should never be MORE
    // restrictive than the old hard-coded 50 MiB.
    let effective_limit = if config.max_attachment_bytes > 0 {
        config
            .max_attachment_bytes
            .max(FALLBACK_MAX_ATTACHMENT_BYTES)
    } else {
        FALLBACK_MAX_ATTACHMENT_BYTES
    };
    let meta = fs::metadata(file_path)?;
    if meta.len() > effective_limit as u64 {
        return Err(StorageError::InvalidPath(format!(
            "Attachment too large ({} bytes, max {})",
            meta.len(),
            effective_limit,
        )));
    }

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
    let project_root = archive_project_root_checked(archive)?;
    let attach_dir = project_root.join("attachments");
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
    if original_bytes.len() > effective_limit {
        return Err(StorageError::InvalidPath(format!(
            "Attachment too large for conversion ({} bytes, max {})",
            original_bytes.len(),
            effective_limit,
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

    atomic_write_bytes(&webp_path, &webp_bytes, true)?;
    let webp_rel = rel_path_cached(&archive.canonical_repo_root, &webp_path)?;
    rel_paths.push(webp_rel.clone());

    // Optionally keep original
    let original_rel = if config.keep_original_images {
        let orig_dir = attach_dir.join("originals").join(prefix);
        ensure_dir(&orig_dir)?;
        let orig_path = orig_dir.join(format!("{digest}{original_ext}"));
        atomic_write_bytes(&orig_path, &original_bytes, true)?;
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
    write_json(&manifest_path, &serde_json::to_value(&manifest)?, true)?;
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

/// Store a raw attachment (no conversion) in the archive.
///
/// Copies the file to `attachments/files/{sha1[:2]}/{sha1}.{ext}`.
/// Returns metadata and relative paths for git commit.
pub fn store_raw_attachment(
    archive: &ProjectArchive,
    file_path: &Path,
    max_bytes: usize,
) -> Result<StoredAttachment> {
    let effective_limit = if max_bytes > 0 {
        max_bytes.max(FALLBACK_MAX_ATTACHMENT_BYTES)
    } else {
        FALLBACK_MAX_ATTACHMENT_BYTES
    };
    // Check size before reading entire file to prevent OOM
    let meta = fs::metadata(file_path)?;
    if meta.len() > effective_limit as u64 {
        return Err(StorageError::InvalidPath(format!(
            "Attachment too large ({} bytes, max {})",
            meta.len(),
            effective_limit,
        )));
    }

    // Read file
    let bytes = fs::read(file_path)?;
    if bytes.is_empty() {
        return Err(StorageError::InvalidPath(
            "Attachment file is empty".to_string(),
        ));
    }

    // Compute SHA1
    let digest = {
        let mut hasher = sha1::Sha1::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    };

    let prefix = &digest[..2.min(digest.len())];
    let ext = file_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();

    let project_root = archive_project_root_checked(archive)?;
    let attach_dir = project_root.join("attachments");
    let file_dir = attach_dir.join("files").join(prefix);
    ensure_dir(&file_dir)?;

    let filename = format!("{digest}{ext}");
    let target_path = file_dir.join(&filename);

    if path_existing_prefix_has_symlink(&target_path)? {
        return Err(StorageError::InvalidPath(format!(
            "raw attachment target path must not include symlinks: {}",
            target_path.display()
        )));
    }

    let needs_write = match fs::symlink_metadata(&target_path) {
        Ok(meta) if meta.file_type().is_file() => fs::read(&target_path)? != bytes,
        Ok(_) => {
            return Err(StorageError::InvalidPath(format!(
                "raw attachment target is not a regular file: {}",
                target_path.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => return Err(err.into()),
    };

    if needs_write {
        atomic_write_bytes(&target_path, &bytes, true)?;
    }

    let rel_path = rel_path_cached(&archive.canonical_repo_root, &target_path)?;

    let meta = AttachmentMeta {
        kind: "file".to_string(),
        media_type: "application/octet-stream".to_string(),
        bytes: bytes.len(),
        sha1: digest,
        width: 0,
        height: 0,
        data_base64: None,
        path: Some(rel_path.clone()),
        original_path: None,
    };

    Ok(StoredAttachment {
        meta,
        rel_paths: vec![rel_path],
    })
}

/// Process attachment paths and store them in the archive.
///
/// Resolves paths sequentially (fast), then converts up to
/// `MAX_CONCURRENT_CONVERSIONS` attachments in parallel using
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
    if attachment_paths.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let canonical_base = canonicalize_path_cached(base_dir).map_err(|e| {
        StorageError::InvalidPath(format!(
            "Attachment base directory does not exist or is not accessible: {} ({e})",
            base_dir.display()
        ))
    })?;

    // Phase 1: resolve all paths (fast, no image I/O)
    let resolved: Vec<PathBuf> = attachment_paths
        .iter()
        .map(|p| resolve_attachment_source_path_from_canonical_base(&canonical_base, config, p))
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
                .map(|h| h.join().unwrap_or_else(|_| unreachable!()))
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
    RE.get_or_init(|| {
        Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<path>[^)]+)\)").unwrap_or_else(|_| unreachable!())
    })
}

/// Process inline image references in Markdown body.
///
/// Finds `![alt](path)` references and converts them in parallel (up to
/// `MAX_CONCURRENT_CONVERSIONS` at a time) with either:
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

    let canonical_base = match canonicalize_path_cached(base_dir) {
        Ok(b) => b,
        Err(_) => return Ok((body_md.to_string(), Vec::new(), Vec::new())),
    };

    // Collect and filter matches: (full_match, alt, resolved_path)
    let processable: Vec<(String, String, PathBuf)> = re
        .captures_iter(body_md)
        .filter_map(|cap| {
            let full = cap
                .get(0)
                .unwrap_or_else(|| unreachable!())
                .as_str()
                .to_string();
            let alt = cap
                .name("alt")
                .unwrap_or_else(|| unreachable!())
                .as_str()
                .to_string();
            let path = cap
                .name("path")
                .unwrap_or_else(|| unreachable!())
                .as_str()
                .to_string();

            // Skip data URIs and URLs
            if path.starts_with("data:")
                || path.starts_with("http://")
                || path.starts_with("https://")
            {
                return None;
            }

            // Resolve best-effort: missing/unresolvable paths don't fail the message.
            let resolved = resolve_source_attachment_path_opt(&canonical_base, config, &path)?;
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
                .map(|h| h.join().unwrap_or_else(|_| unreachable!()))
                .collect()
        });
        converted.extend(chunk_results);
    }

    // Apply replacements
    let mut result = body_md.to_string();
    let mut all_meta = Vec::new();
    let mut all_rel_paths = Vec::new();
    let mut seen_attachment_fingerprints = std::collections::HashSet::new();

    for (full_match, alt, stored_result) in converted {
        let stored = stored_result?;
        let replacement = if let Some(ref b64) = stored.meta.data_base64 {
            format!("![{alt}](data:image/webp;base64,{b64})")
        } else if let Some(ref file_path) = stored.meta.path {
            format!("![{alt}]({file_path})")
        } else {
            continue;
        };
        result = result.replace(&full_match, &replacement);
        all_rel_paths.extend(stored.rel_paths);

        // Strip data_base64 to avoid duplicating the payload in the attachments JSON array,
        // since the base64 string is already fully embedded in the markdown body above.
        let mut meta = stored.meta;
        meta.data_base64 = None;
        let fingerprint = (
            meta.sha1.clone(),
            meta.kind.clone(),
            meta.path.clone(),
            meta.original_path.clone(),
        );
        if seen_attachment_fingerprints.insert(fingerprint) {
            all_meta.push(meta);
        }
    }
    let mut seen_rel_paths = std::collections::HashSet::new();
    all_rel_paths.retain(|path| seen_rel_paths.insert(path.clone()));

    Ok((result, all_meta, all_rel_paths))
}

/// Return `true` when the Markdown body contains at least one local image
/// reference that would be materialized into the archive.
pub fn markdown_has_processable_local_images(
    config: &Config,
    base_dir: &Path,
    body_md: &str,
) -> bool {
    let canonical_base = match canonicalize_path_cached(base_dir) {
        Ok(b) => b,
        Err(_) => return false,
    };
    image_pattern_re().captures_iter(body_md).any(|cap| {
        let path = cap.name("path").unwrap_or_else(|| unreachable!()).as_str();
        if path.starts_with("data:") || path.starts_with("http://") || path.starts_with("https://")
        {
            return false;
        }
        resolve_source_attachment_path_opt(&canonical_base, config, path).is_some()
    })
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
    let base = canonicalize_path_cached(base_dir).map_err(|e| {
        StorageError::InvalidPath(format!(
            "Attachment base directory does not exist or is not accessible: {} ({e})",
            base_dir.display()
        ))
    })?;

    resolve_attachment_source_path_from_canonical_base(&base, config, raw_path)
}

/// Resolve an attachment source path using a base directory that is already
/// canonicalized.
///
/// This additive helper exists for hot paths that need to resolve many
/// attachment candidates against the same project root without paying the
/// base-directory `canonicalize()` cost on every path.
#[doc(hidden)]
pub fn resolve_attachment_source_path_from_canonical_base(
    canonical_base: &Path,
    config: &Config,
    raw_path: &str,
) -> Result<PathBuf> {
    let raw = raw_path.trim();
    if raw.is_empty() {
        return Err(StorageError::InvalidPath(
            "Attachment path cannot be empty".to_string(),
        ));
    }

    let input = PathBuf::from(raw);
    let input_is_absolute = input.is_absolute();
    let candidate = if input_is_absolute {
        input
    } else {
        canonical_base.join(input)
    };

    let resolved = match candidate.canonicalize() {
        Ok(resolved) => resolved,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::InvalidPath(format!(
                "Attachment not found: {}",
                candidate.display()
            )));
        }
        Err(e) => {
            return Err(StorageError::InvalidPath(format!(
                "Invalid attachment path: {} ({e})",
                candidate.display()
            )));
        }
    };

    // Relative paths must never escape the project directory.
    if !input_is_absolute && !resolved.starts_with(canonical_base) {
        return Err(StorageError::InvalidPath(format!(
            "Attachment path escapes the project directory: {}",
            resolved.display()
        )));
    }

    if resolved.starts_with(canonical_base) {
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
    canonical_base: &Path,
    config: &Config,
    raw_path: &str,
) -> Option<PathBuf> {
    let raw = raw_path.trim();
    if raw.is_empty() {
        return None;
    }

    let input = PathBuf::from(raw);
    let input_is_absolute = input.is_absolute();
    let candidate = if input_is_absolute {
        input
    } else {
        canonical_base.join(input)
    };
    let resolved = candidate.canonicalize().ok()?;

    // Relative paths must never escape the project directory.
    if !input_is_absolute && !resolved.starts_with(canonical_base) {
        return None;
    }

    if resolved.starts_with(canonical_base) {
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

type SignalDebounceKey = (PathBuf, String, String);

static SIGNAL_DEBOUNCE: OnceLock<OrderedMutex<HashMap<SignalDebounceKey, u128>>> = OnceLock::new();

fn signal_debounce() -> &'static OrderedMutex<HashMap<SignalDebounceKey, u128>> {
    SIGNAL_DEBOUNCE
        .get_or_init(|| OrderedMutex::new(LockLevel::StorageSignalDebounce, HashMap::new()))
}

fn signal_debounce_key(config: &Config, project_slug: &str, agent_name: &str) -> SignalDebounceKey {
    (
        config.notifications_signals_dir.clone(),
        project_slug.to_string(),
        agent_name.to_string(),
    )
}

/// Emit a notification signal file for a project/agent.
pub fn emit_notification_signal(
    config: &Config,
    project_slug: &str,
    agent_name: &str,
    message_metadata: Option<&NotificationMessage>,
) -> SignalEmitOutcome {
    if !config.notifications_enabled {
        return SignalEmitOutcome::Disabled;
    }

    // Reject empty, whitespace-only, or path-traversal values to prevent writing
    // outside the signals directory.
    if project_slug.trim().is_empty()
        || agent_name.trim().is_empty()
        || project_slug.contains('/')
        || project_slug.contains('\\')
        || project_slug.contains("..")
        || project_slug == "."
        || project_slug.starts_with('.')
        || agent_name.contains('/')
        || agent_name.contains('\\')
        || agent_name.contains("..")
        || agent_name == "."
        || agent_name.starts_with('.')
    {
        return SignalEmitOutcome::InvalidTarget;
    }

    let debounce_ms = config.notifications_debounce_ms as u128;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let key = signal_debounce_key(config, project_slug, agent_name);
    {
        let mut map = signal_debounce().lock();
        let last = map.get(&key).copied().unwrap_or(0);
        if debounce_ms > 0 && now_ms.saturating_sub(last) < debounce_ms {
            return SignalEmitOutcome::Debounced;
        }
        map.insert(key.clone(), now_ms);
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

    if config.notifications_include_metadata
        && let Some(meta) = message_metadata
    {
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

    match write_json(&signal_path, &signal_data, false) {
        Ok(()) => SignalEmitOutcome::Emitted,
        Err(_) => {
            let mut map = signal_debounce().lock();
            if map.get(&key).copied() == Some(now_ms) {
                map.remove(&key);
            }
            SignalEmitOutcome::WriteFailed
        }
    }
}

/// Clear notification signal for a project/agent.
pub fn clear_notification_signal(
    config: &Config,
    project_slug: &str,
    agent_name: &str,
) -> SignalClearOutcome {
    if !config.notifications_enabled {
        return SignalClearOutcome::Disabled;
    }

    // Reject path-traversal characters and dot-prefix names.
    if project_slug.trim().is_empty()
        || agent_name.trim().is_empty()
        || project_slug.contains('/')
        || project_slug.contains('\\')
        || project_slug.contains("..")
        || project_slug == "."
        || project_slug.starts_with('.')
        || agent_name.contains('/')
        || agent_name.contains('\\')
        || agent_name.contains("..")
        || agent_name == "."
        || agent_name.starts_with('.')
    {
        return SignalClearOutcome::InvalidTarget;
    }

    let signal_path = config
        .notifications_signals_dir
        .join("projects")
        .join(project_slug)
        .join("agents")
        .join(format!("{agent_name}.signal"));
    let signal_key = signal_debounce_key(config, project_slug, agent_name);
    let parent = signal_path.parent().unwrap_or(Path::new("."));

    if path_existing_prefix_has_symlink(parent).unwrap_or(true) {
        return SignalClearOutcome::RemoveFailed;
    }
    if let Ok(meta) = fs::symlink_metadata(&signal_path)
        && meta.file_type().is_symlink()
    {
        return SignalClearOutcome::RemoveFailed;
    }

    if !signal_path.exists() {
        signal_debounce().lock().remove(&signal_key);
        return SignalClearOutcome::Missing;
    }

    match fs::remove_file(&signal_path) {
        Ok(()) => {
            signal_debounce().lock().remove(&signal_key);
            SignalClearOutcome::Cleared
        }
        Err(_) => SignalClearOutcome::RemoveFailed,
    }
}

/// List pending notification signals.
pub fn list_pending_signals(config: &Config, project_slug: Option<&str>) -> Vec<serde_json::Value> {
    if !config.notifications_enabled {
        return Vec::new();
    }

    let projects_root = config.notifications_signals_dir.join("projects");
    if path_existing_prefix_has_symlink(&projects_root).unwrap_or(true) {
        return Vec::new();
    }
    if !fs::symlink_metadata(&projects_root).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut dirs: Vec<PathBuf> = if let Some(slug) = project_slug {
        // Reject path-traversal characters.
        if slug.trim().is_empty()
            || slug.contains('/')
            || slug.contains('\\')
            || slug.contains("..")
            || slug == "."
            || slug.starts_with('.')
        {
            return Vec::new();
        }
        let d = projects_root.join(slug);
        if fs::symlink_metadata(&d).is_ok_and(|meta| meta.file_type().is_dir()) {
            vec![d]
        } else {
            vec![]
        }
    } else {
        match fs::read_dir(&projects_root) {
            Ok(iter) => iter
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let file_type = entry.file_type().ok()?;
                    if !file_type.is_dir() || file_type.is_symlink() {
                        return None;
                    }
                    Some(entry.path())
                })
                .collect(),
            Err(_) => return Vec::new(),
        }
    };
    dirs.sort();

    for proj_dir in dirs {
        let slug = proj_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let agents_dir = proj_dir.join("agents");
        if !fs::symlink_metadata(&agents_dir).is_ok_and(|meta| meta.file_type().is_dir()) {
            continue;
        }
        let mut entries: Vec<_> = match fs::read_dir(&agents_dir) {
            Ok(iter) => iter.filter_map(|entry| entry.ok()).collect(),
            Err(_) => continue,
        };
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
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
    let repo = open_archive_repo_checked(archive)?;
    let path_filter = path_filter
        .map(|filter| validate_repo_relative_path("path filter", filter))
        .transpose()?;

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
    let filter = Path::new(path_prefix);
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return false,
    };

    // Check if any entry in the diff starts with path_prefix
    if commit.parent_count() == 0 {
        // Root commit: check all entries
        return tree_contains_prefix(&tree, filter);
    }

    if let Ok(parent) = commit.parent(0)
        && let Ok(parent_tree) = parent.tree()
        && let Ok(diff) = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
    {
        let mut found = false;
        let _ = diff.foreach(
            &mut |delta, _progress| {
                let touches_new = delta
                    .new_file()
                    .path()
                    .is_some_and(|path| repo_path_matches_filter(path, filter));
                let touches_old = delta
                    .old_file()
                    .path()
                    .is_some_and(|path| repo_path_matches_filter(path, filter));
                if touches_new || touches_old {
                    found = true;
                }
                true
            },
            None,
            None,
            None,
        );
        return found;
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
    let repo = open_archive_repo_checked(archive)?;
    let rel_path_str = validate_repo_relative_path("repo-relative path", rel_path_str)?;

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
    let repo = open_archive_repo_checked(archive)?;

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
    // Use saturating_sub so a pathologically negative/future commit timestamp
    // (e.g., corrupt or crafted `author.when().seconds()`) cannot panic in
    // debug builds or wrap in release builds.
    let delta = now.saturating_sub(authored_secs);
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
    reject_symlinked_archive_root(archive_root)?;
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

    reject_symlinked_archive_root(archive_root)?;
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
    let project_slug = validate_archive_component("project slug", &archive.slug)?;

    let repo = open_archive_repo_checked(archive)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let commit = head.peel_to_commit()?;
    let root_tree = commit.tree()?;

    // Navigate to projects/{slug}/{path}
    let tree_path = if safe_path.is_empty() {
        format!("projects/{project_slug}")
    } else {
        format!("projects/{project_slug}/{safe_path}")
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
        b_dir.cmp(&a_dir).then_with(|| {
            a.name
                .bytes()
                .map(|b| b.to_ascii_lowercase())
                .cmp(b.name.bytes().map(|b| b.to_ascii_lowercase()))
        })
    });

    Ok(entries)
}

/// Sanitize a browsable path to prevent directory traversal.
fn sanitize_browse_path(path: &str) -> Result<String> {
    let normalized = path.replace('\\', "/");
    if std::path::Path::new(&normalized).is_absolute()
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
    let project_slug = validate_archive_component("project slug", &archive.slug)?;
    if safe_path.is_empty() {
        return Ok(None);
    }

    let repo = open_archive_repo_checked(archive)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let commit = head.peel_to_commit()?;
    let root_tree = commit.tree()?;

    let full_path = format!("projects/{project_slug}/{safe_path}");

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

/// Get a historical snapshot of an agent inbox at a target timestamp.
///
/// Returns a JSON object with:
/// - `messages`: list of message summaries
/// - `snapshot_time`: commit time used (or null)
/// - `commit_sha`: commit hash used (or null)
/// - `requested_time`: original timestamp request
/// - optional `note` or `error` fields
pub fn get_historical_inbox_snapshot(
    archive: &ProjectArchive,
    agent_name: &str,
    timestamp: &str,
    limit: usize,
) -> Result<serde_json::Value> {
    let limit = limit.clamp(1, 500);
    let project_slug = validate_archive_component("project slug", &archive.slug)?;
    let agent_name = validate_archive_component("agent name", agent_name)?;

    let target_seconds = {
        // First try RFC3339 variants, then naive datetime forms.
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            dt.with_timezone(&Utc).timestamp()
        } else if let Ok(naive) =
            chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.f")
        {
            naive.and_utc().timestamp()
        } else if let Ok(naive) =
            chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S")
        {
            naive.and_utc().timestamp()
        } else if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M")
        {
            naive.and_utc().timestamp()
        } else {
            return Ok(serde_json::json!({
                "messages": [],
                "snapshot_time": serde_json::Value::Null,
                "commit_sha": serde_json::Value::Null,
                "requested_time": timestamp,
                "error": "Invalid timestamp format",
            }));
        }
    };

    let repo = open_archive_repo_checked(archive)?;
    let inbox_path = format!("projects/{project_slug}/agents/{agent_name}/inbox");

    let mut closest_oid: Option<git2::Oid> = None;

    // Pass 1: commits touching this inbox path.
    {
        let mut revwalk = repo.revwalk()?;
        if revwalk.push_head().is_ok() {
            revwalk.set_sorting(git2::Sort::TIME)?;
            for oid_result in revwalk {
                let oid = oid_result?;
                let commit = repo.find_commit(oid)?;
                if !commit_touches_path(&repo, &commit, &inbox_path) {
                    continue;
                }
                if commit.time().seconds() <= target_seconds {
                    closest_oid = Some(oid);
                    break;
                }
            }
        }
    }

    // Pass 2: fallback to global history if inbox path has no commits before target.
    if closest_oid.is_none() {
        let mut revwalk = repo.revwalk()?;
        if revwalk.push_head().is_ok() {
            revwalk.set_sorting(git2::Sort::TIME)?;
            for oid_result in revwalk {
                let oid = oid_result?;
                let commit = repo.find_commit(oid)?;
                if commit.time().seconds() <= target_seconds {
                    closest_oid = Some(oid);
                    break;
                }
            }
        }
    }

    let Some(closest_oid) = closest_oid else {
        return Ok(serde_json::json!({
            "messages": [],
            "snapshot_time": serde_json::Value::Null,
            "commit_sha": serde_json::Value::Null,
            "requested_time": timestamp,
            "note": "No commits found before this timestamp",
        }));
    };

    let commit = repo.find_commit(closest_oid)?;
    let mut messages: Vec<serde_json::Value> = Vec::new();

    fn collect_historical_inbox_messages(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        depth: usize,
        limit: usize,
        messages: &mut Vec<serde_json::Value>,
    ) {
        if messages.len() >= limit || depth > 3 {
            return;
        }

        let entries: Vec<_> = tree.iter().collect();
        for item in entries.into_iter().rev() {
            if messages.len() >= limit {
                break;
            }

            match item.kind() {
                Some(git2::ObjectType::Tree) if depth < 3 => {
                    if let Ok(child_tree) = repo.find_tree(item.id()) {
                        collect_historical_inbox_messages(
                            repo,
                            &child_tree,
                            depth + 1,
                            limit,
                            messages,
                        );
                    }
                }
                Some(git2::ObjectType::Blob) => {
                    let Some(name) = item.name() else {
                        continue;
                    };
                    if !name.ends_with(".md") {
                        continue;
                    }

                    let mut from_agent = "unknown".to_string();
                    let mut importance = "normal".to_string();

                    let filename = name.strip_suffix(".md").unwrap_or(name);
                    let parts: Vec<&str> = filename.rsplitn(3, "__").collect();
                    let (date_str, subject_slug, msg_id) = match parts.as_slice() {
                        [id, subject, date] => (*date, *subject, *id),
                        [subject, date] => (*date, *subject, "unknown"),
                        _ => continue,
                    };

                    let mut subject = subject_slug.replace(['-', '_'], " ").trim().to_string();
                    if subject.is_empty() {
                        subject = "Unknown".to_string();
                    }

                    if let Ok(blob) = repo.find_blob(item.id()) {
                        let blob_content = String::from_utf8_lossy(blob.content()).to_string();
                        if let Some(rest) = blob_content.strip_prefix("---json\n")
                            && let Some(end_idx) = rest.find("\n---\n")
                        {
                            let json_str = &rest[..end_idx];
                            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_str) {
                                if let Some(from) =
                                    meta.get("from").and_then(serde_json::Value::as_str)
                                {
                                    from_agent = from.to_string();
                                }
                                if let Some(imp) =
                                    meta.get("importance").and_then(serde_json::Value::as_str)
                                {
                                    importance = imp.to_string();
                                }
                                if let Some(subj) = meta
                                    .get("subject")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    subject = subj.to_string();
                                }
                            }
                        }
                    }

                    messages.push(serde_json::json!({
                        "id": msg_id,
                        "subject": subject,
                        "date": date_str,
                        "from": from_agent,
                        "importance": importance,
                    }));
                }
                _ => {}
            }
        }
    }

    if let Ok(root_tree) = commit.tree()
        && let Ok(inbox_entry) = root_tree.get_path(Path::new(&inbox_path))
        && let Ok(inbox_tree) = repo.find_tree(inbox_entry.id())
    {
        collect_historical_inbox_messages(&repo, &inbox_tree, 0, limit, &mut messages);
    }

    messages.sort_by(|a, b| {
        let a_date = a
            .get("date")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let b_date = b
            .get("date")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        b_date.cmp(a_date)
    });

    let snapshot_time = DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0)
        .map_or_else(String::new, |dt| dt.to_rfc3339());

    Ok(serde_json::json!({
        "messages": messages,
        "snapshot_time": snapshot_time,
        "commit_sha": closest_oid.to_string(),
        "requested_time": timestamp,
    }))
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
    let project_slug = validate_archive_component("project slug", project_slug)?;
    reject_symlinked_archive_root(archive_root)?;
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

        let message = commit.message().unwrap_or("");
        for line in message.lines() {
            let line = line.trim_start_matches("- ").trim();
            if !line.starts_with("mail: ") {
                continue;
            }

            // Parse "mail: Sender -> Recipient1, Recipient2 | Subject"
            let rest = &line[6..];
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
    let project_slug = validate_archive_component("project slug", project_slug)?;
    reject_symlinked_archive_root(archive_root)?;
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
        } else if subject.starts_with("batch: ") {
            ("batch".to_string(), None, Vec::new())
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
    if path_existing_prefix_has_symlink(path)? {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to read message through symlinked path: {}",
                path.display()
            ),
        )));
    }

    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("message path is not a regular file: {}", path.display()),
        )));
    }
    if meta.len() > 50 * 1024 * 1024 {
        // 50MB safety limit
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "message file {} exceeds maximum size of 50MB",
                path.display()
            ),
        )));
    }

    let content = fs::read_to_string(path)?;

    // Parse ---json frontmatter
    if let Some(rest) = content.strip_prefix("---json\n")
        && let Some(end_idx) = rest.find("\n---\n")
    {
        let json_str = &rest[..end_idx];
        let body = rest[end_idx + 5..]
            .strip_prefix("\r\n")
            .or_else(|| rest[end_idx + 5..].strip_prefix('\n'))
            .unwrap_or(&rest[end_idx + 5..])
            .to_string();
        let frontmatter = serde_json::from_str(json_str)?;
        return Ok((frontmatter, body));
    }

    // No frontmatter - treat entire content as body
    Ok((serde_json::Value::Null, content))
}

/// List all message files in a directory (inbox, outbox, or canonical).
///
/// Returns paths sorted by modification time (newest first).
pub fn list_message_files(dir: &Path) -> Result<Vec<PathBuf>> {
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
    if path_existing_prefix_has_symlink(dir)? || !path_is_nonsymlink_dir(dir) {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_md_files(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|e| e == "md") {
            files.push(path);
        }
    }
    Ok(())
}

/// Get inbox message files for a specific agent.
pub fn list_agent_inbox(archive: &ProjectArchive, agent_name: &str) -> Result<Vec<PathBuf>> {
    let agent_name = validate_archive_component("agent name", agent_name)?;
    let project_root = archive_project_root_checked(archive)?;
    let inbox_dir = project_root.join("agents").join(agent_name).join("inbox");
    list_message_files(&inbox_dir)
}

/// Get outbox message files for a specific agent.
pub fn list_agent_outbox(archive: &ProjectArchive, agent_name: &str) -> Result<Vec<PathBuf>> {
    let agent_name = validate_archive_component("agent name", agent_name)?;
    let project_root = archive_project_root_checked(archive)?;
    let outbox_dir = project_root.join("agents").join(agent_name).join("outbox");
    list_message_files(&outbox_dir)
}

/// List all agents with profiles in the archive.
pub fn list_archive_agents(archive: &ProjectArchive) -> Result<Vec<String>> {
    let agents_dir = archive_project_root_checked(archive)?.join("agents");
    if path_existing_prefix_has_symlink(&agents_dir).unwrap_or(true) {
        return Ok(Vec::new());
    }

    let mut agents = Vec::new();
    let iter = match fs::read_dir(&agents_dir) {
        Ok(i) => i,
        Err(_) => return Ok(Vec::new()),
    };
    for entry in iter {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() && !file_type.is_symlink() {
            let profile = entry.path().join("profile.json");
            if path_is_nonsymlink_file(&profile)
                && let Some(name) = entry.file_name().to_str()
            {
                agents.push(name.to_string());
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
    let profile_path = archive_project_root_checked(archive)?
        .join("agents")
        .join(agent_name)
        .join("profile.json");
    if path_existing_prefix_has_symlink(profile_path.parent().unwrap_or(Path::new(".")))
        .unwrap_or(true)
        || !path_is_nonsymlink_file(&profile_path)
    {
        return Ok(None);
    }
    let content = fs::read_to_string(&profile_path)?;
    let value = serde_json::from_str(&content)?;
    Ok(Some(value))
}

fn repo_path_matches_filter(candidate: &Path, filter: &Path) -> bool {
    candidate == filter || candidate.starts_with(filter)
}

/// Check if a tree has any entry with the given repo-relative path prefix.
fn tree_contains_prefix(tree: &git2::Tree<'_>, prefix: &Path) -> bool {
    let mut found = false;
    let _ = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        let Some(name) = entry.name() else {
            return git2::TreeWalkResult::Ok;
        };
        let candidate = if root.is_empty() {
            PathBuf::from(name)
        } else {
            Path::new(root).join(name)
        };
        if repo_path_matches_filter(&candidate, prefix) {
            found = true;
            return git2::TreeWalkResult::Abort;
        }
        git2::TreeWalkResult::Ok
    });
    found
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
        if path_existing_prefix_has_symlink(dir)? || !path_is_nonsymlink_dir(dir) {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk_locks(&path, locks)?;
            } else if file_type.is_file() && path.extension().is_some_and(|e| e == "lock") {
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

                // Check for owner metadata (read directly — avoids TOCTOU with exists())
                let owner_path = path.with_extension("lock.owner.json");
                if path_is_nonsymlink_file(&owner_path)
                    && let Ok(content) = fs::read_to_string(&owner_path)
                    && let Ok(owner) = serde_json::from_str::<serde_json::Value>(&content)
                    && let Some(obj) = locks.last_mut().and_then(|v| v.as_object_mut())
                {
                    obj.insert("owner".to_string(), owner);
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
///
/// Previously had a `git -C <workdir> read-tree HEAD` shell-out as a
/// "try CLI git first, fall back to libgit2" path. Removed under bead
/// br-8ujfs.3.5 (C5): the CLI git branch was redundant with the
/// libgit2 recovery below (`reset_index_to_head` + `index.write`) and
/// added a race surface against the 2.51.0 `.git/index` bug. The
/// libgit2 path handles every case we care about — regular repos,
/// unborn HEAD (via `try_clear_for_empty_head`), detached HEAD,
/// submodules. See `docs/GIT_SHELLOUT_AUDIT.md` #2.
fn try_restore_index_to_head(repo: &Repository) -> Result<()> {
    if let Some(workdir) = repo.workdir() {
        // Heal stale lock artifacts so the libgit2 write has a clean
        // `.git/index` to acquire.
        let _ = try_clean_stale_git_lock(workdir, 300.0);
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
    let _workdir = repo.workdir().ok_or(StorageError::NotInitialized)?;

    let mut index = repo.index()?;
    reset_index_to_head(repo, &mut index)?;
    let mut any_added = false;

    for path in rel_paths {
        let path = validate_repo_relative_path("commit path", path)?;
        // git2 expects forward-slash paths on all platforms
        let p = Path::new(path);

        match index.add_path(p) {
            Ok(()) => {}
            Err(err) if err.code() == git2::ErrorCode::NotFound => {
                // File doesn't exist on disk, remove it from the index (deletion/move).
                // Ignore "path not in index" errors, but propagate others.
                if let Err(remove_err) = index.remove_path(p)
                    && remove_err.code() != git2::ErrorCode::NotFound
                {
                    return Err(remove_err.into());
                }
            }
            Err(err) => {
                // `add_path` already performed the existence/readability check.
                // Keep richer errors for non-NotFound failures.
                return Err(err.into());
            }
        }
        any_added = true;
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

    if message.starts_with("mail: ")
        && !has_agent
        && let Some(rest) = message.strip_prefix("mail: ")
        && let Some(agent_part) = rest.split("->").next()
    {
        let agent = agent_part.trim();
        if !agent.is_empty() {
            trailers.push(format!("Agent: {agent}"));
        }
    } else if message.starts_with("file_reservation: ")
        && !has_agent
        && let Some(rest) = message.strip_prefix("file_reservation: ")
        && let Some(agent_part) = rest.split_whitespace().next()
    {
        let agent = agent_part.trim();
        if !agent.is_empty() {
            trailers.push(format!("Agent: {agent}"));
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
/// `readlink` syscalls).  The target is canonicalized via `fs::canonicalize()`
/// to resolve any symlinks before comparison, preventing symlink-based path
/// traversal attacks. For paths that do not yet exist, we canonicalize the
/// nearest existing ancestor and append the missing suffix so symlinked parent
/// directories are still resolved correctly.
fn rel_path_cached(canonical_base: &Path, target: &Path) -> Result<String> {
    let canonical_target = match target.canonicalize() {
        Ok(p) => p,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            canonicalize_with_existing_prefix(target).map_err(StorageError::Io)?
        }
        Err(err) => return Err(StorageError::Io(err)),
    };

    canonical_target
        .strip_prefix(canonical_base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| {
            StorageError::InvalidPath(format!(
                "Cannot compute relative path from {} to {}",
                canonical_base.display(),
                canonical_target.display()
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

    if normalized.is_empty() || std::path::Path::new(&normalized).is_absolute() {
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
    if safe_rel.is_empty() {
        return Err(StorageError::InvalidPath(
            "path must not be empty or root".to_string(),
        ));
    }
    let root = archive_project_root_canonical_checked(archive)?;

    // Manually normalize the path components to prevent traversal
    // via `foo/../../../etc/passwd` patterns, avoiding the high syscall
    // overhead of `canonicalize()` which issues `readlink` per component.
    let mut candidate = root.clone();
    for component in Path::new(safe_rel).components() {
        match component {
            Component::Normal(c) => candidate.push(c),
            Component::CurDir => { /* skip `.` */ }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(StorageError::InvalidPath(
                    "directory traversal not allowed".to_string(),
                ));
            }
        }
    }

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
fn write_text(path: &Path, content: &str, sync: bool) -> Result<()> {
    ensure_parent_dir(path)?;
    atomic_write_bytes(path, content.as_bytes(), sync)
}

/// Write JSON content to a file atomically (write-to-temp-then-rename).
///
/// Creates parent directories as needed.
fn write_json(path: &Path, value: &serde_json::Value, sync: bool) -> Result<()> {
    ensure_parent_dir(path)?;
    let content = serde_json::to_string_pretty(value)?;
    atomic_write_bytes(path, content.as_bytes(), sync)
}

static ATOMIC_WRITE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static ATOMIC_WRITE_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static ATOMIC_WRITE_TEST_LOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct AtomicWriteTestGuard {
    lock: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
fn atomic_write_test_guard() -> AtomicWriteTestGuard {
    let reentrant = ATOMIC_WRITE_TEST_LOCK_DEPTH.with(|depth| {
        let current = depth.get();
        depth.set(current + 1);
        current > 0
    });
    let lock = if reentrant {
        None
    } else {
        Some(
            ATOMIC_WRITE_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("atomic write test lock poisoned"),
        )
    };
    AtomicWriteTestGuard { lock }
}

#[cfg(test)]
impl Drop for AtomicWriteTestGuard {
    fn drop(&mut self) {
        drop(self.lock.take());
        ATOMIC_WRITE_TEST_LOCK_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

fn atomic_write_tmp_path(parent: &Path, path: &Path, seq: u64) -> PathBuf {
    let name_hash = {
        use std::hash::{Hash, Hasher};
        let mut s = std::collections::hash_map::DefaultHasher::new();
        path.file_name().hash(&mut s);
        s.finish()
    };
    let tmp_name = format!(".tmp-{}-{:016x}-{seq}", std::process::id(), name_hash);
    parent.join(tmp_name)
}

/// Write bytes to a file atomically via a temp file + rename.
///
/// The temp file is created in the same directory as the target so that
/// `fs::rename` is guaranteed to be atomic (same filesystem).
fn atomic_write_bytes(path: &Path, data: &[u8], sync: bool) -> Result<()> {
    use std::io::Write as _;

    #[cfg(test)]
    let _test_guard = atomic_write_test_guard();

    // Refuse to write through symlinks (prevents symlink escape attacks)
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing to write through symlink: {}", path.display()),
            )));
        }
    }

    let parent = path.parent().unwrap_or(Path::new("."));
    if path_existing_prefix_has_symlink(parent)? {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to write through symlinked parent path: {}",
                parent.display()
            ),
        )));
    }

    // Use pid + seq + filename-hash for a unique temp name. Open with
    // create_new so pre-existing files or symlinks cannot be clobbered.
    for _ in 0..64 {
        let seq = ATOMIC_WRITE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = atomic_write_tmp_path(parent, path, seq);

        let result: std::io::Result<()> = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            f.write_all(data)?;
            if sync {
                f.sync_data()?;
            }
            fs::rename(&tmp_path, path)?;
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                // Best-effort cleanup of temp files we created. If the path
                // already existed before our open, leave it untouched.
                let _ = fs::remove_file(&tmp_path);
                return Err(err.into());
            }
        }
    }

    Err(StorageError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate unique temp file for atomic write in {}",
            parent.display()
        ),
    )))
}

fn canonicalize_with_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing_suffix = Vec::new();
    let mut current = path;

    loop {
        match current.canonicalize() {
            Ok(canonical_current) => {
                let mut resolved = canonical_current;
                for component in missing_suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(err);
                };
                missing_suffix.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(err);
                };
                current = parent;
            }
            Err(err) => return Err(err),
        }
    }
}

fn path_is_nonsymlink_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

fn path_is_nonsymlink_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

fn reject_symlinked_archive_root(archive_root: &Path) -> Result<()> {
    if path_existing_prefix_has_symlink(archive_root)? {
        return Err(StorageError::InvalidPath(
            "archive root must not be symlinked".to_string(),
        ));
    }
    Ok(())
}

fn open_archive_repo_checked(archive: &ProjectArchive) -> Result<Repository> {
    Repository::open(archive_repo_root_checked(archive)?).map_err(StorageError::from)
}

fn archive_repo_root_checked(archive: &ProjectArchive) -> Result<&Path> {
    // `canonical_repo_root` is the pre-resolved form we actually operate on,
    // but `repo_root` is a `pub` field that external callers (including the
    // legacy shell installer and some migration paths) can mutate directly.
    // Validate both so a post-construction mutation to `repo_root` (e.g.
    // retargeting at a symlinked mount) cannot slip past the
    // "archive root must not be symlinked" guard.
    reject_symlinked_archive_root(&archive.canonical_repo_root)?;
    reject_symlinked_archive_root(&archive.repo_root)?;
    Ok(&archive.canonical_repo_root)
}

fn archive_project_root_checked(archive: &ProjectArchive) -> Result<PathBuf> {
    let project_slug = validate_archive_component("project slug", &archive.slug)?;
    let project_root = archive_repo_root_checked(archive)?
        .join("projects")
        .join(project_slug);
    if path_existing_prefix_has_symlink(&project_root)? {
        return Err(StorageError::InvalidPath(
            "project archive root must not be symlinked".to_string(),
        ));
    }
    Ok(project_root)
}

fn archive_project_root_canonical_checked(archive: &ProjectArchive) -> Result<PathBuf> {
    let project_root = archive_project_root_checked(archive)?;
    canonicalize_with_existing_prefix(&project_root).map_err(|err| {
        StorageError::InvalidPath(format!(
            "project archive root is not accessible: {} ({err})",
            project_root.display()
        ))
    })
}

fn path_existing_prefix_has_symlink(path: &Path) -> std::io::Result<bool> {
    let mut current = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => continue,
            Component::ParentDir => current.push(".."),
            Component::Normal(part) => current.push(part),
        }

        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(false)
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
        let project_slug = match validate_archive_component("project slug", &msg.project_slug) {
            Ok(project_slug) => project_slug,
            Err(_) => {
                missing += 1;
                if missing_ids.len() < 20 {
                    missing_ids.push(msg.message_id);
                }
                continue;
            }
        };
        // Build the expected canonical path:
        // {storage_root}/projects/{slug}/messages/{YYYY}/{MM}/{iso}__{slug}__{id}.md
        let project_dir = storage_root.join("projects").join(project_slug);

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

        let iso_prefix = archive_filename_timestamp_prefix(&msg.created_ts_iso);
        let subject_slug = slugify_message_subject(&msg.subject);
        let fallback_subject_marker = format!("__{subject_slug}");

        let messages_dir = project_dir.join("messages").join(&year).join(&month);

        // Look for a file matching the pattern: {iso}__{slug}__{id}.md
        // We check both the computed path and do a directory scan fallback
        // because the archive ID suffix can vary.
        //
        // After `doctor reconstruct`, DB IDs may have been remapped from
        // canonical frontmatter IDs, so the `__{id}.md` suffix in the
        // filename might not match the current DB row ID. To avoid
        // false-positive "missing" reports, we also accept same-second files
        // whose filename matches the subject slug and whose frontmatter still
        // matches the DB row's subject, sender, and created second.
        // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/10
        let found_file = if path_existing_prefix_has_symlink(&messages_dir).unwrap_or(true) {
            false
        } else if path_is_nonsymlink_dir(&messages_dir) {
            let id_suffix = format!("__{}.md", msg.message_id);
            match std::fs::read_dir(&messages_dir) {
                Ok(entries) => entries.flatten().any(|entry| {
                    let Ok(file_type) = entry.file_type() else {
                        return false;
                    };
                    if file_type.is_symlink() || !file_type.is_file() {
                        return false;
                    }
                    let path = entry.path();
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    // Primary match: timestamp prefix + exact DB ID suffix
                    if name_str.starts_with(&iso_prefix) && name_str.ends_with(&id_suffix) {
                        return true;
                    }
                    archive_fallback_matches_message_ref(
                        &path,
                        &name_str,
                        msg,
                        &iso_prefix,
                        &fallback_subject_marker,
                    )
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

fn archive_filename_timestamp_prefix(iso: &str) -> String {
    let iso_filename = iso.replace(':', "-").replace('+', "");
    if iso_filename.len() >= 19 {
        iso_filename[..19].to_string()
    } else {
        iso_filename
    }
}

fn archive_fallback_matches_message_ref(
    path: &Path,
    entry_name: &str,
    msg: &ConsistencyMessageRef,
    expected_iso_prefix: &str,
    fallback_subject_marker: &str,
) -> bool {
    if !entry_name.starts_with(expected_iso_prefix)
        || !entry_name.contains(fallback_subject_marker)
        || !entry_name.ends_with(".md")
    {
        return false;
    }

    let Ok((frontmatter, _body)) = read_message_file(path) else {
        return false;
    };
    let Some(frontmatter_subject) = frontmatter
        .get("subject")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let Some(frontmatter_sender) = frontmatter.get("from").and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let Some(frontmatter_created) = frontmatter
        .get("created")
        .or_else(|| frontmatter.get("created_ts"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };

    frontmatter_subject == msg.subject
        && frontmatter_sender == msg.sender_name
        && archive_filename_timestamp_prefix(frontmatter_created) == expected_iso_prefix
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
    use chrono::Datelike;
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

    #[test]
    fn coalescer_flush_interval_clamps_zero_duration() {
        assert_eq!(
            clamp_coalescer_flush_interval(Duration::ZERO),
            MIN_COALESCER_FLUSH_INTERVAL
        );
    }

    #[test]
    fn coalescer_flush_interval_preserves_reasonable_values() {
        let interval = Duration::from_millis(50);
        assert_eq!(clamp_coalescer_flush_interval(interval), interval);
    }

    #[test]
    fn configured_coalescer_flush_interval_reads_env_override() {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ARCHIVE_BATCH_MS", "250")],
            || {
                assert_eq!(
                    configured_coalescer_flush_interval(),
                    Duration::from_millis(250)
                );
            },
        );
    }

    #[test]
    fn configured_coalescer_batch_size_reads_env_override() {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ARCHIVE_BATCH_EVENTS", "64")],
            || {
                assert_eq!(configured_coalescer_batch_size(), 64);
            },
        );
    }

    fn test_repo_queue() -> RepoQueue {
        RepoQueue {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            spill: std::sync::Mutex::new(CoalescerSpillState::default()),
            depth: AtomicU64::new(0),
            processing: AtomicBool::new(false),
            last_serviced_us: AtomicU64::new(0),
            metrics: RepoCommitMetrics::default(),
        }
    }

    #[test]
    fn coalescer_repo_readiness_waits_until_flush_window_or_batch_threshold() {
        let rq = test_repo_queue();
        {
            let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(CoalescerCommitFields {
                enqueued_at: Instant::now(),
                enqueued_wall: Utc::now(),
                git_author_name: "Ready".to_string(),
                git_author_email: "ready@example.com".to_string(),
                message: "one".to_string(),
                rel_paths: vec!["one.txt".to_string()],
            });
        }
        rq.depth.store(1, Ordering::Relaxed);

        let readiness = coalescer_repo_readiness(&rq, 2, Duration::from_secs(3_600), false);
        assert!(
            matches!(readiness, CoalescerRepoReadiness::Waiting(wait) if wait > Duration::ZERO),
            "single fresh event should wait for the flush window, got {readiness:?}"
        );
        assert_eq!(
            coalescer_repo_readiness(&rq, 2, Duration::from_secs(3_600), true),
            CoalescerRepoReadiness::Ready
        );

        {
            let mut q = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(CoalescerCommitFields {
                enqueued_at: Instant::now(),
                enqueued_wall: Utc::now(),
                git_author_name: "Ready".to_string(),
                git_author_email: "ready@example.com".to_string(),
                message: "two".to_string(),
                rel_paths: vec!["two.txt".to_string()],
            });
        }
        rq.depth.store(2, Ordering::Relaxed);

        assert_eq!(
            coalescer_repo_readiness(&rq, 2, Duration::from_secs(3_600), false),
            CoalescerRepoReadiness::Ready
        );
    }

    #[test]
    fn coalescer_batch_commit_message_uses_event_count_and_wall_range() {
        let early = DateTime::parse_from_rfc3339("2026-04-21T04:00:00Z")
            .expect("valid early timestamp")
            .with_timezone(&Utc);
        let late = DateTime::parse_from_rfc3339("2026-04-21T04:00:02Z")
            .expect("valid late timestamp")
            .with_timezone(&Utc);
        let mk_req = |message: &str, ts| CoalescerCommitFields {
            enqueued_at: Instant::now(),
            enqueued_wall: ts,
            git_author_name: "Batch".to_string(),
            git_author_email: "batch@example.com".to_string(),
            message: message.to_string(),
            rel_paths: vec![format!("{message}.txt")],
        };

        assert_eq!(
            coalescer_batch_commit_message(&[mk_req("late", late), mk_req("early", early)]),
            "batch: 2 events (2026-04-21T04:00:00Z – 2026-04-21T04:00:02Z)"
        );
    }

    fn unique_human_key(prefix: &str) -> String {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        format!("/tmp/{prefix}-{suffix}")
    }

    fn enqueue_with_retry(op_template: WriteOp, context: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut attempts = 0_u32;
        loop {
            attempts = attempts.saturating_add(1);
            match wbq_enqueue(op_template.clone()) {
                WbqEnqueueResult::Enqueued => return,
                WbqEnqueueResult::SkippedDiskCritical => {
                    if std::time::Instant::now() >= deadline {
                        panic!(
                            "{context}: enqueue remained skipped due critical disk pressure \
                             after {attempts} attempts"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                WbqEnqueueResult::QueueUnavailable => {
                    if std::time::Instant::now() >= deadline {
                        panic!("{context}: enqueue remained unavailable after {attempts} attempts");
                    }
                    wbq_flush();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
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
    fn test_open_archive_returns_existing_project() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let created = ensure_archive(&config, "test-project").unwrap();
        let opened = open_archive(&config, "test-project")
            .unwrap()
            .expect("archive should open");

        assert_eq!(opened.slug, created.slug);
        assert_eq!(opened.root, created.root);
    }

    #[cfg(unix)]
    #[test]
    fn test_open_archive_rejects_symlinked_project_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let projects_root = config.storage_root.join("projects");
        let outside_project = tmp.path().join("outside-project");
        fs::create_dir_all(&projects_root).unwrap();
        fs::create_dir_all(&outside_project).unwrap();
        symlink(&outside_project, projects_root.join("linked-proj")).unwrap();

        assert!(
            open_archive(&config, "linked-proj").unwrap().is_none(),
            "symlinked project directories must not be opened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_open_archive_rejects_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let real_root = tmp.path().join("real-root");
        let linked_root = tmp.path().join("linked-root");
        fs::create_dir_all(real_root.join("projects").join("proj")).unwrap();
        symlink(&real_root, &linked_root).unwrap();

        let mut config = test_config(tmp.path());
        config.storage_root = linked_root;

        assert!(
            open_archive(&config, "proj").unwrap().is_none(),
            "symlinked storage roots must not be traversed during archive open"
        );
    }

    #[test]
    fn canonicalize_path_cached_reuses_absolute_path_result() {
        let tmp = TempDir::new().unwrap();
        let canonical = canonicalize_path_cached(tmp.path()).unwrap();
        assert_eq!(canonical, tmp.path().canonicalize().unwrap());

        let cached = canonicalize_path_cached(tmp.path()).unwrap();
        assert_eq!(cached, canonical);
    }

    #[test]
    fn test_write_project_metadata_with_config() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        write_project_metadata_with_config(&archive, &config, "/tmp/proj").unwrap();
        let metadata_path = archive.root.join("project.json");
        assert!(metadata_path.exists());

        let metadata: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&metadata_path).unwrap()).unwrap();
        assert_eq!(metadata["slug"], "proj");
        assert_eq!(metadata["human_key"], "/tmp/proj");

        // Rewriting with the same data should remain a no-op and still succeed.
        write_project_metadata_with_config(&archive, &config, "/tmp/proj").unwrap();
    }

    #[test]
    fn test_write_project_metadata_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");

        archive.root = forged_root.clone();
        write_project_metadata_with_config(&archive, &config, "/tmp/proj").unwrap();

        assert!(real_root.join("project.json").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive project metadata writes"
        );
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
    fn test_write_agent_profile_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");

        archive.root = forged_root.clone();

        let agent = serde_json::json!({
            "name": "TestAgent",
            "program": "test",
            "model": "test-model",
        });

        write_agent_profile_with_config(&archive, &config, &agent).unwrap();

        assert!(real_root.join("agents/TestAgent/profile.json").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive profile writes"
        );
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
    fn test_write_file_reservation_record_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");

        archive.root = forged_root.clone();

        let reservation = serde_json::json!({
            "id": 42,
            "agent": "TestAgent",
            "path_pattern": "src/**/*.rs",
            "exclusive": true,
        });

        write_file_reservation_record(&archive, &config, &reservation).unwrap();

        assert!(real_root.join("file_reservations/id-42.json").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive reservation writes"
        );
    }

    #[test]
    fn db_and_storage_message_pipeline_consistent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let db_path = tmp.path().join("message_pipeline.db");
        let pool_config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root: Some(config.storage_root.clone()),
            max_connections: 8,
            min_connections: 2,
            acquire_timeout_ms: 60_000,
            max_lifetime_ms: 3_600_000,
            run_migrations: true,
            warmup_connections: 0,
            cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
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
            storage_root: Some(config.storage_root.clone()),
            max_connections: 8,
            min_connections: 2,
            acquire_timeout_ms: 60_000,
            max_lifetime_ms: 3_600_000,
            run_migrations: true,
            warmup_connections: 0,
            cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
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
    fn write_file_reservation_records_validates_the_full_batch_before_writing() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let reservations = vec![
            serde_json::json!({
                "id": 1,
                "agent": "GreenCastle",
                "path_pattern": "src/**",
                "exclusive": true,
            }),
            serde_json::json!({
                "id": 2,
                "agent": "GreenCastle",
                "exclusive": true,
            }),
        ];

        let err = write_file_reservation_records(&archive, &config, &reservations)
            .expect_err("invalid batch should fail");
        assert!(err.to_string().contains("path_pattern"));

        let reservation_dir = archive.root.join("file_reservations");
        if reservation_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&reservation_dir)
                .unwrap()
                .flatten()
                .collect();
            assert!(
                entries.is_empty(),
                "failed reservation batches must not leave partial archive artifacts"
            );
        }
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
    fn test_write_message_bundle_keeps_bcc_inbox_private_from_thread_digest() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 2,
            "subject": "Private Message",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-BCC",
            "project": "proj",
            "to": ["VisibleAgent"],
            "cc": [],
            "bcc": ["HiddenAgent"],
        });

        write_message_bundle(
            &archive,
            &config,
            &message,
            "Hello world!",
            "SenderAgent",
            &["VisibleAgent".to_string(), "HiddenAgent".to_string()],
            &[],
            None,
        )
        .unwrap();

        let hidden_inbox_dir = archive.root.join("agents/HiddenAgent/inbox/2026/01");
        assert!(
            hidden_inbox_dir.exists(),
            "bcc recipient should still get inbox copy"
        );

        let canonical_path = fs::read_dir(archive.root.join("messages/2026/01"))
            .unwrap()
            .find_map(|entry| entry.ok().map(|entry| entry.path()))
            .expect("canonical path");
        let outbox_path = fs::read_dir(archive.root.join("agents/SenderAgent/outbox/2026/01"))
            .unwrap()
            .find_map(|entry| entry.ok().map(|entry| entry.path()))
            .expect("outbox path");
        let hidden_inbox_path = fs::read_dir(&hidden_inbox_dir)
            .unwrap()
            .find_map(|entry| entry.ok().map(|entry| entry.path()))
            .expect("hidden inbox path");

        let (canonical_frontmatter, _) = read_message_file(&canonical_path).unwrap();
        assert_eq!(
            canonical_frontmatter["bcc"],
            serde_json::json!(["HiddenAgent"])
        );

        let (outbox_frontmatter, _) = read_message_file(&outbox_path).unwrap();
        assert_eq!(
            outbox_frontmatter["bcc"],
            serde_json::json!(["HiddenAgent"])
        );

        let (hidden_inbox_frontmatter, _) = read_message_file(&hidden_inbox_path).unwrap();
        assert_eq!(hidden_inbox_frontmatter["bcc"], serde_json::json!([]));

        let digest = archive.root.join("messages/threads/tkt-bcc.md");
        let digest_body = fs::read_to_string(&digest).unwrap();
        assert!(
            digest_body.contains("VisibleAgent"),
            "visible recipients should still appear in thread digest"
        );
        assert!(
            !digest_body.contains("HiddenAgent"),
            "bcc recipients must not leak into thread digest"
        );
    }

    #[test]
    fn test_write_message_bundle_preserves_body_whitespace_exactly() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 1,
            "subject": "Whitespace Fidelity",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-WS",
            "project": "proj",
        });
        let body = "  leading space\nline 2\n\ntrailing space  ";

        write_message_bundle(
            &archive,
            &config,
            &message,
            body,
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();

        let msg_dir = archive.root.join("messages/2026/01");
        let canonical_path = fs::read_dir(&msg_dir)
            .unwrap()
            .find_map(|entry| entry.ok().map(|entry| entry.path()))
            .expect("canonical message path");

        let expected_content = format!(
            "---json\n{}\n---\n\n{}",
            serde_json::to_string_pretty(&message).unwrap(),
            body
        );
        let raw = fs::read_to_string(&canonical_path).unwrap();
        assert_eq!(raw, expected_content);

        let (frontmatter, parsed_body) = read_message_file(&canonical_path).unwrap();
        assert_eq!(frontmatter["subject"], "Whitespace Fidelity");
        assert_eq!(parsed_body, body);
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
    fn test_write_message_bundle_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");

        archive.root = forged_root.clone();

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

        assert!(real_root.join("messages/2026/01").exists());
        assert!(real_root.join("agents/SenderAgent/outbox/2026/01").exists());
        assert!(
            real_root
                .join("agents/RecipientAgent/inbox/2026/01")
                .exists()
        );
        assert!(real_root.join("messages/threads/tkt-1.md").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive message writes"
        );
    }

    #[test]
    fn test_write_message_bundle_ignores_forged_archive_repo_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let real_root = archive.root.clone();
        let forged_repo_root = tmp.path().join("outside-repo");

        archive.repo_root = forged_repo_root.clone();

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

        assert!(real_root.join("messages/2026/01").exists());
        assert!(
            !forged_repo_root.exists(),
            "forged archive.repo_root must not receive message writes"
        );
    }

    fn snapshot_archive_files(root: &Path) -> std::collections::BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, String>) {
            let mut entries: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .collect();
            entries.sort();
            for path in entries {
                if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, out);
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let content = fs::read_to_string(&path).unwrap();
                out.insert(rel, content);
            }
        }

        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn test_write_message_batch_bundle_matches_single_message_layout() {
        let tmp_single = TempDir::new().unwrap();
        let config_single = test_config(tmp_single.path());
        let archive_single = ensure_archive(&config_single, "proj").unwrap();

        let tmp_batch = TempDir::new().unwrap();
        let config_batch = test_config(tmp_batch.path());
        let archive_batch = ensure_archive(&config_batch, "proj").unwrap();

        let messages = vec![
            serde_json::json!({
                "id": 1,
                "subject": "Batch Layout One",
                "created_ts": "2026-01-15T10:00:00Z",
                "thread_id": "TKT-BATCH",
                "project": "proj",
            }),
            serde_json::json!({
                "id": 2,
                "subject": "Batch Layout Two",
                "created_ts": "2026-01-15T10:01:00Z",
                "thread_id": "TKT-BATCH",
                "project": "proj",
            }),
        ];
        let recipients = vec!["RecipientAgent".to_string()];

        for message in &messages {
            write_message_bundle(
                &archive_single,
                &config_single,
                message,
                "Hello world!",
                "SenderAgent",
                &recipients,
                &[],
                None,
            )
            .unwrap();
        }
        flush_async_commits();

        let batch_entries = messages
            .iter()
            .map(|message| MessageBundleBatchEntry {
                message,
                body_md: "Hello world!",
                sender: "SenderAgent",
                recipients: &recipients,
                extra_paths: &[],
            })
            .collect::<Vec<_>>();
        write_message_batch_bundle(&archive_batch, &config_batch, &batch_entries, None).unwrap();
        flush_async_commits();

        assert_eq!(
            snapshot_archive_files(&archive_single.root),
            snapshot_archive_files(&archive_batch.root)
        );
    }

    #[test]
    fn test_write_message_batch_bundle_keeps_bcc_inbox_private_from_thread_digest() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let message = serde_json::json!({
            "id": 2,
            "subject": "Private Batch Message",
            "created_ts": "2026-01-15T10:00:00Z",
            "thread_id": "TKT-BCC-BATCH",
            "project": "proj",
            "to": ["VisibleAgent"],
            "cc": [],
            "bcc": ["HiddenAgent"],
        });
        let recipients = vec!["VisibleAgent".to_string(), "HiddenAgent".to_string()];
        let batch_entries = [MessageBundleBatchEntry {
            message: &message,
            body_md: "Hello world!",
            sender: "SenderAgent",
            recipients: &recipients,
            extra_paths: &[],
        }];

        write_message_batch_bundle(&archive, &config, &batch_entries, None).unwrap();
        flush_async_commits();

        let hidden_inbox_dir = archive.root.join("agents/HiddenAgent/inbox/2026/01");
        assert!(
            hidden_inbox_dir.exists(),
            "bcc recipient should still get inbox copy"
        );

        let hidden_inbox_path = fs::read_dir(&hidden_inbox_dir)
            .unwrap()
            .find_map(|entry| entry.ok().map(|entry| entry.path()))
            .expect("hidden inbox path");
        let (hidden_inbox_frontmatter, _) = read_message_file(&hidden_inbox_path).unwrap();
        assert_eq!(hidden_inbox_frontmatter["bcc"], serde_json::json!([]));

        let digest = archive.root.join("messages/threads/tkt-bcc-batch.md");
        let digest_body = fs::read_to_string(&digest).unwrap();
        assert!(digest_body.contains("VisibleAgent"));
        assert!(!digest_body.contains("HiddenAgent"));
    }

    #[test]
    fn test_write_message_batch_bundle_uses_single_commit_summary() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();

        let messages = [
            serde_json::json!({
                "id": 1,
                "subject": "Commit Summary One",
                "created_ts": "2026-01-15T10:00:00Z",
                "thread_id": "TKT-COMMIT",
                "project": "proj",
            }),
            serde_json::json!({
                "id": 2,
                "subject": "Commit Summary Two",
                "created_ts": "2026-01-15T10:01:00Z",
                "thread_id": "TKT-COMMIT",
                "project": "proj",
            }),
        ];
        let recipients = vec!["RecipientAgent".to_string()];
        let batch_entries = messages
            .iter()
            .map(|message| MessageBundleBatchEntry {
                message,
                body_md: "Hello world!",
                sender: "SenderAgent",
                recipients: &recipients,
                extra_paths: &[],
            })
            .collect::<Vec<_>>();

        write_message_batch_bundle(&archive, &config, &batch_entries, None).unwrap();
        flush_async_commits();

        let commits = get_recent_commits(&archive, 3, None).unwrap();
        assert_eq!(commits[0].summary, "batch: 2 message bundles");
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
    fn test_resolve_archive_relative_path_uses_validated_project_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "proj").unwrap();
        let other = ensure_archive(&config, "other").unwrap();

        archive.slug = "other".to_string();
        let resolved = resolve_archive_relative_path(&archive, "target.txt").unwrap();
        assert!(resolved.starts_with(&other.root));
        assert!(!resolved.starts_with(tmp.path().join("projects").join("proj")));
    }

    #[cfg(unix)]
    #[test]
    fn rel_path_cached_rejects_missing_target_under_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();
        let outside = outside_tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, archive.root.join("escape")).unwrap();

        let target = archive.root.join("escape").join("message.md");
        let err = rel_path_cached(&archive.canonical_repo_root, &target)
            .expect_err("symlinked parent should be rejected");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[cfg(unix)]
    #[test]
    fn write_text_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proj").unwrap();
        let outside = outside_tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, archive.root.join("escape")).unwrap();

        let target = archive.root.join("escape").join("message.md");
        let err =
            write_text(&target, "payload", false).expect_err("symlinked parent should be rejected");
        assert!(
            err.to_string().contains("symlinked path")
                || err.to_string().contains("symlinked parent"),
            "unexpected error: {err}"
        );
        assert!(
            !outside.join("message.md").exists(),
            "write should not escape outside the archive root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_text_skips_preexisting_temp_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let target = tmp.path().join("message.md");
        let outside = outside_tmp.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();

        let _guard = atomic_write_test_guard();
        ATOMIC_WRITE_TMP_COUNTER.store(0, Ordering::Relaxed);
        for seq in 0..8 {
            let tmp_path = atomic_write_tmp_path(tmp.path(), &target, seq);
            symlink(&outside, &tmp_path).unwrap();
        }

        write_text(&target, "payload", false).expect("preexisting temp symlinks should be skipped");

        assert_eq!(fs::read_to_string(&target).unwrap(), "payload");
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "outside",
            "atomic write must not follow attacker-controlled temp symlinks"
        );
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
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", Some(&meta)),
            SignalEmitOutcome::Emitted
        );

        let signals = list_pending_signals(&config, Some("proj"));
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(signal["project"], "proj");
        assert_eq!(signal["agent"], "Agent");
        assert!(signal["timestamp"].as_str().is_some());
        assert_eq!(signal["message"]["id"], 123);
        assert_eq!(signal["message"]["importance"], "high");

        let cleared = clear_notification_signal(&config, "proj", "Agent");
        assert_eq!(cleared, SignalClearOutcome::Cleared);

        let signals2 = list_pending_signals(&config, Some("proj"));
        assert!(signals2.is_empty());
    }

    #[test]
    fn test_clear_notification_signal_resets_debounce_state() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 60_000;

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            clear_notification_signal(&config, "proj", "Agent"),
            SignalClearOutcome::Cleared
        );
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Emitted,
            "clearing should allow the next real notification immediately"
        );
    }

    #[test]
    fn test_emit_notification_signal_reports_debounced_state() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 60_000;

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Debounced
        );
    }

    #[test]
    fn test_signal_debounce_is_scoped_to_signal_root() {
        let tmp = TempDir::new().unwrap();
        let mut config_a = test_config(tmp.path());
        config_a.notifications_enabled = true;
        config_a.notifications_signals_dir = tmp.path().join("signals-a");
        config_a.notifications_debounce_ms = 60_000;

        let mut config_b = test_config(tmp.path());
        config_b.notifications_enabled = true;
        config_b.notifications_signals_dir = tmp.path().join("signals-b");
        config_b.notifications_debounce_ms = 60_000;

        assert_eq!(
            emit_notification_signal(&config_a, "proj", "Agent", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config_b, "proj", "Agent", None),
            SignalEmitOutcome::Emitted,
            "debounce state must not leak across distinct signal roots"
        );
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
        assert_eq!(
            emit_notification_signal(&config, "test_project", "TestAgent", Some(&meta)),
            SignalEmitOutcome::Emitted
        );

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
        assert_eq!(
            emit_notification_signal(&config, "proj1", "Agent1", Some(&meta)),
            SignalEmitOutcome::Emitted
        );

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
        assert_eq!(
            emit_notification_signal(&config, "proj1", "Agent2", Some(&meta)),
            SignalEmitOutcome::Emitted
        );

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
        assert_eq!(
            emit_notification_signal(&config, "test_project", "TestAgent", Some(&meta)),
            SignalEmitOutcome::Emitted
        );

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

        assert_eq!(
            emit_notification_signal(&config, "test_project", "TestAgent", None),
            SignalEmitOutcome::Emitted
        );

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

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Disabled
        );
        let signals = list_pending_signals(&config, None);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_signal_failed_write_does_not_poison_debounce_state() {
        let tmp = TempDir::new().unwrap();
        let blocked_root = tmp.path().join("blocked-signals");
        fs::write(&blocked_root, "not a directory").unwrap();

        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_debounce_ms = 60_000;
        config.notifications_signals_dir = blocked_root;

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::WriteFailed,
            "first emit should fail when the configured signal root is not a directory"
        );

        config.notifications_signals_dir = tmp.path().join("signals");
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Emitted,
            "a failed write must not reserve debounce state and suppress the retry"
        );
        assert_eq!(list_pending_signals(&config, Some("proj")).len(), 1);
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
        assert_eq!(
            emit_notification_signal(&config, "proj1", "Agent1", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj1", "Agent2", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj2", "Agent1", None),
            SignalEmitOutcome::Emitted
        );

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
    fn test_signal_list_is_sorted_deterministically() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = false;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        assert_eq!(
            emit_notification_signal(&config, "proj2", "AgentB", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj1", "AgentC", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj1", "AgentA", None),
            SignalEmitOutcome::Emitted
        );

        let all = list_pending_signals(&config, None);
        let ordered: Vec<(String, String)> = all
            .iter()
            .map(|signal| {
                (
                    signal["project"].as_str().unwrap_or_default().to_string(),
                    signal["agent"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("proj1".to_string(), "AgentA".to_string()),
                ("proj1".to_string(), "AgentC".to_string()),
                ("proj2".to_string(), "AgentB".to_string()),
            ]
        );
    }

    #[test]
    fn test_signal_clear_and_relist() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_include_metadata = false;
        config.notifications_signals_dir = tmp.path().join("signals");
        config.notifications_debounce_ms = 0;

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent1", None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent2", None),
            SignalEmitOutcome::Emitted
        );

        // Clear Agent1
        assert_eq!(
            clear_notification_signal(&config, "proj", "Agent1"),
            SignalClearOutcome::Cleared
        );
        let signals = list_pending_signals(&config, Some("proj"));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0]["agent"], "Agent2");

        // Clear nonexistent returns false
        assert_eq!(
            clear_notification_signal(&config, "proj", "NonExistent"),
            SignalClearOutcome::Missing
        );
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
        assert_eq!(
            emit_notification_signal(&config, "../evil", agent, None),
            SignalEmitOutcome::InvalidTarget
        );
        assert_eq!(
            emit_notification_signal(&config, "proj/sub", agent, None),
            SignalEmitOutcome::InvalidTarget
        );

        // Backslash in project_slug
        assert_eq!(
            emit_notification_signal(&config, "proj\\sub", agent, None),
            SignalEmitOutcome::InvalidTarget
        );

        // Dot-dot in project_slug
        assert_eq!(
            emit_notification_signal(&config, "proj..", agent, None),
            SignalEmitOutcome::InvalidTarget
        );
        assert_eq!(
            emit_notification_signal(&config, "..proj", agent, None),
            SignalEmitOutcome::InvalidTarget
        );

        // Slash in agent_name
        assert_eq!(
            emit_notification_signal(&config, slug, "../evil", None),
            SignalEmitOutcome::InvalidTarget
        );
        assert_eq!(
            emit_notification_signal(&config, slug, "Agent/sub", None),
            SignalEmitOutcome::InvalidTarget
        );

        // Backslash in agent_name
        assert_eq!(
            emit_notification_signal(&config, slug, "Agent\\sub", None),
            SignalEmitOutcome::InvalidTarget
        );

        // clear_notification_signal rejects the same patterns
        assert_eq!(
            clear_notification_signal(&config, "../evil", agent),
            SignalClearOutcome::InvalidTarget
        );
        assert_eq!(
            clear_notification_signal(&config, "", agent),
            SignalClearOutcome::InvalidTarget
        );
        assert_eq!(
            clear_notification_signal(&config, slug, "   "),
            SignalClearOutcome::InvalidTarget
        );
        assert_eq!(
            clear_notification_signal(&config, slug, "../evil"),
            SignalClearOutcome::InvalidTarget
        );
        assert_eq!(
            clear_notification_signal(&config, "proj\\sub", agent),
            SignalClearOutcome::InvalidTarget
        );
        assert_eq!(
            clear_notification_signal(&config, slug, "Agent\\sub"),
            SignalClearOutcome::InvalidTarget
        );

        // list_pending_signals rejects traversal in slug
        assert!(list_pending_signals(&config, Some("")).is_empty());
        assert!(list_pending_signals(&config, Some("   ")).is_empty());
        assert!(list_pending_signals(&config, Some("../evil")).is_empty());
        assert!(list_pending_signals(&config, Some("proj/sub")).is_empty());
        assert!(list_pending_signals(&config, Some("proj\\sub")).is_empty());

        // Legitimate names still work
        assert_eq!(
            emit_notification_signal(&config, slug, agent, None),
            SignalEmitOutcome::Emitted
        );
        assert_eq!(list_pending_signals(&config, Some(slug)).len(), 1);
        assert_eq!(
            clear_notification_signal(&config, slug, agent),
            SignalClearOutcome::Cleared
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_clear_notification_signal_refuses_symlinked_project_path_and_preserves_debounce() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let safe_signals = tmp.path().join("safe-signals");
        let linked_signals = tmp.path().join("linked-signals");
        let outside_project = tmp.path().join("outside-project");

        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_debounce_ms = 60_000;
        config.notifications_signals_dir = safe_signals.clone();

        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Emitted
        );

        let outside_agents = outside_project.join("agents");
        std::fs::create_dir_all(&outside_agents).unwrap();
        std::fs::write(
            outside_agents.join("Agent.signal"),
            "{\"project\":\"proj\",\"agent\":\"Agent\"}",
        )
        .unwrap();
        let linked_project = linked_signals.join("projects").join("proj");
        std::fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        symlink(&outside_project, &linked_project).unwrap();

        config.notifications_signals_dir = linked_signals;
        assert_eq!(
            clear_notification_signal(&config, "proj", "Agent"),
            SignalClearOutcome::RemoveFailed
        );
        assert!(
            outside_agents.join("Agent.signal").exists(),
            "failed clear must not delete through a symlinked project path"
        );

        config.notifications_signals_dir = safe_signals;
        assert_eq!(
            emit_notification_signal(&config, "proj", "Agent", None),
            SignalEmitOutcome::Debounced,
            "failed clear must not reset debounce state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_clear_notification_signal_refuses_symlinked_signal_file() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_debounce_ms = 0;
        config.notifications_signals_dir = tmp.path().join("signals");

        let agents_dir = config
            .notifications_signals_dir
            .join("projects")
            .join("proj")
            .join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let outside_signal = tmp.path().join("outside.signal");
        std::fs::write(
            &outside_signal,
            "{\"project\":\"proj\",\"agent\":\"Agent\"}",
        )
        .unwrap();
        let linked_signal = agents_dir.join("Agent.signal");
        symlink(&outside_signal, &linked_signal).unwrap();

        assert_eq!(
            clear_notification_signal(&config, "proj", "Agent"),
            SignalClearOutcome::RemoveFailed
        );
        assert!(
            linked_signal.exists(),
            "symlink leaf should remain untouched"
        );
        assert!(
            outside_signal.exists(),
            "rejecting a symlink leaf must not disturb the external target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_signal_list_skips_symlinked_project_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        let outside_project = tmp.path().join("outside-project");
        let outside_agents = outside_project.join("agents");
        std::fs::create_dir_all(&outside_agents).unwrap();
        std::fs::write(
            outside_agents.join("Ghost.signal"),
            serde_json::json!({
                "timestamp": "2026-04-10T12:00:00Z",
                "project": "outside",
                "agent": "Ghost",
            })
            .to_string(),
        )
        .unwrap();

        let linked_project = config
            .notifications_signals_dir
            .join("projects")
            .join("linked");
        std::fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        symlink(&outside_project, &linked_project).unwrap();

        assert!(
            list_pending_signals(&config, None).is_empty(),
            "symlinked project directories must not be traversed"
        );
        assert!(
            list_pending_signals(&config, Some("linked")).is_empty(),
            "explicit slug reads must also ignore symlinked project directories"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_signal_list_skips_symlinked_agents_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        let project_dir = config
            .notifications_signals_dir
            .join("projects")
            .join("proj");
        let outside_agents = tmp.path().join("outside-agents");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&outside_agents).unwrap();
        std::fs::write(
            outside_agents.join("Ghost.signal"),
            serde_json::json!({
                "timestamp": "2026-04-10T12:00:00Z",
                "project": "outside",
                "agent": "Ghost",
            })
            .to_string(),
        )
        .unwrap();
        symlink(&outside_agents, project_dir.join("agents")).unwrap();

        assert!(
            list_pending_signals(&config, None).is_empty(),
            "symlinked agents directories must not be traversed"
        );
        assert!(
            list_pending_signals(&config, Some("proj")).is_empty(),
            "explicit slug reads must also ignore symlinked agents directories"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_signal_list_skips_symlinked_signal_file() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.notifications_enabled = true;
        config.notifications_signals_dir = tmp.path().join("signals");

        let agents_dir = config
            .notifications_signals_dir
            .join("projects")
            .join("proj")
            .join("agents");
        let outside_signal = tmp.path().join("outside.signal");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            &outside_signal,
            serde_json::json!({
                "timestamp": "2026-04-10T12:00:00Z",
                "project": "outside",
                "agent": "Ghost",
            })
            .to_string(),
        )
        .unwrap();
        symlink(&outside_signal, agents_dir.join("Ghost.signal")).unwrap();

        assert!(
            list_pending_signals(&config, None).is_empty(),
            "symlinked signal files must not be read during global listing"
        );
        assert!(
            list_pending_signals(&config, Some("proj")).is_empty(),
            "symlinked signal files must not be read during per-project listing"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_file_lock_acquire_rejects_symlinked_lock_path() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("symlink.lock");
        let outside = tmp.path().join("outside.lock");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &lock_path).unwrap();

        let mut lock = FileLock::new(lock_path.clone());
        let err = lock.acquire().unwrap_err();
        assert!(
            err.to_string()
                .contains("lock path must not include symlinks")
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(!lock_path.with_file_name("symlink.lock.owner.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_file_lock_acquire_rejects_symlinked_metadata_path() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("metadata.lock");
        let metadata_path = tmp.path().join("metadata.lock.owner.json");
        let outside = tmp.path().join("outside-owner.json");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &metadata_path).unwrap();

        let mut lock = FileLock::new(lock_path.clone());
        let err = lock.acquire().unwrap_err();
        assert!(
            err.to_string()
                .contains("lock metadata path must not include symlinks")
        );
        assert!(
            !lock_path.exists(),
            "failed acquire must not create a lock file"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
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
    fn test_file_lock_stale_cleanup_concurrent_single_winner() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("race.lock");
        let meta_path = tmp.path().join("race.lock.owner.json");

        fs::write(&lock_path, "locked").unwrap();
        let stale_meta = serde_json::json!({
            "pid": 999999999u32,
            "created_ts": 0.0,
        });
        fs::write(&meta_path, stale_meta.to_string()).unwrap();

        let thread_count = 8usize;
        let barrier = Arc::new(std::sync::Barrier::new(thread_count));
        let mut handles = Vec::with_capacity(thread_count);

        for _ in 0..thread_count {
            let barrier = Arc::clone(&barrier);
            let path = lock_path.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let lock = FileLock::new(path);
                lock.cleanup_if_stale().unwrap()
            }));
        }

        let removed_count = handles
            .into_iter()
            .map(|h| h.join().expect("thread join"))
            .filter(|removed| *removed)
            .count();

        assert_eq!(
            removed_count, 1,
            "exactly one contender should report stale-lock removal"
        );
        assert!(!lock_path.exists(), "lock file should be removed");
    }

    #[test]
    fn test_file_lock_stale_cleanup_appears_between_attempts() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("appearing.lock");
        let meta_path = tmp.path().join("appearing.lock.owner.json");
        let lock = FileLock::new(lock_path.clone());

        assert!(!lock.cleanup_if_stale().unwrap());

        fs::write(&lock_path, "locked").unwrap();
        let stale_meta = serde_json::json!({
            "pid": 999999999u32,
            "created_ts": 0.0,
        });
        fs::write(&meta_path, stale_meta.to_string()).unwrap();

        assert!(
            lock.cleanup_if_stale().unwrap(),
            "second scan should clean newly appeared stale lock"
        );
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_file_lock_stale_cleanup_lock_disappears_before_scan() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("vanished.lock");
        let meta_path = tmp.path().join("vanished.lock.owner.json");

        fs::write(&lock_path, "locked").unwrap();
        let stale_meta = serde_json::json!({
            "pid": 999999999u32,
            "created_ts": 0.0,
        });
        fs::write(&meta_path, stale_meta.to_string()).unwrap();
        fs::remove_file(&lock_path).unwrap();

        let lock = FileLock::new(lock_path.clone());
        assert!(
            !lock.cleanup_if_stale().unwrap(),
            "missing lock should be treated as no-op"
        );
        assert!(
            meta_path.exists(),
            "cleanup_if_stale should not remove metadata when lock is already missing"
        );
    }

    #[test]
    fn test_file_lock_stale_cleanup_alive_pid_can_expire_by_timeout() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("alive-expire.lock");
        let meta_path = tmp.path().join("alive-expire.lock.owner.json");

        fs::write(&lock_path, "locked").unwrap();
        let stale_meta = serde_json::json!({
            "pid": std::process::id(),
            "created_ts": 0.0,
        });
        fs::write(&meta_path, stale_meta.to_string()).unwrap();

        let lock = FileLock::new(lock_path.clone()).with_stale_timeout(Duration::from_secs(1));
        assert!(
            lock.cleanup_if_stale().unwrap(),
            "old lock should expire even if PID is currently alive"
        );
        assert!(!lock_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_file_lock_stale_cleanup_permission_denied_returns_false() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let lock_dir = tmp.path().join("readonly");
        fs::create_dir_all(&lock_dir).unwrap();
        let lock_path = lock_dir.join("perm.lock");
        let meta_path = lock_dir.join("perm.lock.owner.json");

        fs::write(&lock_path, "locked").unwrap();
        let stale_meta = serde_json::json!({
            "pid": 999999999u32,
            "created_ts": 0.0,
        });
        fs::write(&meta_path, stale_meta.to_string()).unwrap();

        let mut perms = fs::metadata(&lock_dir).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&lock_dir, perms).unwrap();

        let lock = FileLock::new(lock_path.clone());
        let removed = lock.cleanup_if_stale().unwrap();

        let mut restore = fs::metadata(&lock_dir).unwrap().permissions();
        restore.set_mode(0o700);
        fs::set_permissions(&lock_dir, restore).unwrap();

        assert!(
            !removed,
            "permission failure must not report successful healing"
        );
        assert!(
            lock_path.exists(),
            "lock should remain when removal is denied"
        );
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

    #[test]
    fn test_with_project_lock_ignores_forged_lock_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "lock-proj").unwrap();
        let real_lock_path = archive.lock_path.clone();
        let forged_lock_path = tmp.path().join("outside-locks").join("forged.lock");

        archive.lock_path = forged_lock_path.clone();

        with_project_lock(&archive, || {
            assert!(real_lock_path.exists(), "real project lock should be held");
            assert!(
                !forged_lock_path.exists(),
                "forged archive.lock_path must not be used"
            );
            Ok(())
        })
        .unwrap();

        assert!(
            !real_lock_path.exists(),
            "real project lock should be released"
        );
        assert!(
            !forged_lock_path.exists(),
            "forged archive.lock_path must remain untouched"
        );
    }

    #[test]
    fn test_with_project_lock_ignores_forged_repo_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "lock-proj").unwrap();
        let real_lock_path = archive.lock_path.clone();
        let forged_repo_root = tmp.path().join("outside-repo");
        let forged_lock_path = forged_repo_root.join("projects/lock-proj/.archive.lock");

        archive.repo_root = forged_repo_root.clone();

        with_project_lock(&archive, || {
            assert!(real_lock_path.exists(), "real project lock should be held");
            assert!(
                !forged_lock_path.exists(),
                "forged archive.repo_root must not control lock placement"
            );
            Ok(())
        })
        .unwrap();

        assert!(
            !real_lock_path.exists(),
            "real project lock should be released"
        );
        assert!(
            !forged_repo_root.exists(),
            "forged archive.repo_root must remain untouched"
        );
    }

    #[test]
    fn test_with_project_lock_uses_validated_project_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "lock-proj").unwrap();
        let other = ensure_archive(&config, "other-proj").unwrap();
        let original_lock_path = archive.lock_path.clone();

        archive.slug = "other-proj".to_string();

        with_project_lock(&archive, || {
            assert!(
                other.lock_path.exists(),
                "lock should follow the validated current project slug"
            );
            assert!(
                !original_lock_path.exists(),
                "stale lock_path from the original archive handle must not be used"
            );
            Ok(())
        })
        .unwrap();

        assert!(
            !other.lock_path.exists(),
            "other project lock should be released"
        );
        assert!(
            !original_lock_path.exists(),
            "original project lock path must remain untouched"
        );
    }

    #[test]
    fn test_with_project_lock_rejects_invalid_archive_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "lock-proj").unwrap();

        archive.slug = "../escape".to_string();
        let err = with_project_lock(&archive, || Ok(()))
            .expect_err("invalid project slug should be rejected");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
        assert!(
            !archive.lock_path.exists(),
            "invalid archive slug must not create lock artifacts"
        );
    }

    #[test]
    fn test_with_project_lock_allows_same_slug_across_distinct_roots() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();

        let config_a = test_config(tmp_a.path());
        let config_b = test_config(tmp_b.path());
        let archive_a = Arc::new(ensure_archive(&config_a, "shared-slug").unwrap());
        let archive_b = Arc::new(ensure_archive(&config_b, "shared-slug").unwrap());

        assert_ne!(
            archive_a.root, archive_b.root,
            "distinct storage roots must produce distinct archive roots"
        );

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let worker = |archive: Arc<ProjectArchive>,
                      barrier: Arc<std::sync::Barrier>,
                      active: Arc<AtomicUsize>,
                      max_active: Arc<AtomicUsize>| {
            std::thread::spawn(move || {
                barrier.wait();
                with_project_lock(&archive, || {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now_active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            })
        };

        let handle_a = worker(
            Arc::clone(&archive_a),
            Arc::clone(&barrier),
            Arc::clone(&active),
            Arc::clone(&max_active),
        );
        let handle_b = worker(archive_b, barrier, active, Arc::clone(&max_active));

        handle_a.join().unwrap();
        handle_b.join().unwrap();

        assert_eq!(
            max_active.load(Ordering::SeqCst),
            2,
            "same slug in distinct archive roots should not serialize on the in-process lock"
        );
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

    #[test]
    fn test_commit_queue_normalizes_repo_root_identity() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("archive");
        let mut config = test_config(tmp.path());
        config.storage_root = repo_root.clone();
        let archive = ensure_archive(&config, "normalized-queue-proj").unwrap();
        let repo_root_alias = repo_root.join("alias").join("..");
        fs::create_dir_all(repo_root.join("alias")).unwrap();
        assert_ne!(
            repo_root.as_os_str(),
            repo_root_alias.as_os_str(),
            "test setup requires a distinct textual alias for the same repo"
        );

        let queue = CommitQueue::default();
        for (idx, root_variant) in [repo_root.clone(), repo_root_alias].into_iter().enumerate() {
            let file = archive.root.join(format!("normalized-{idx}.txt"));
            fs::write(&file, format!("content {idx}")).unwrap();
            let rel = rel_path_cached(&archive.canonical_repo_root, &file).unwrap();
            queue
                .enqueue(
                    root_variant,
                    &config,
                    format!("normalized commit {idx}"),
                    vec![rel],
                )
                .unwrap();
        }

        queue.drain(&config).unwrap();

        let stats = queue.stats();
        assert_eq!(stats.enqueued, 2);
        assert_eq!(
            stats.commits, 1,
            "different textual paths to the same repo should batch as one repo"
        );
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

    #[test]
    fn test_heal_archive_locks_never_removes_git_index_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        ensure_archive_root(&config).unwrap();

        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let index_lock = git_dir.join("index.lock");
        fs::write(&index_lock, "active git lock").unwrap();

        let result = heal_archive_locks(&config).unwrap();
        assert!(index_lock.exists(), "git index.lock must be preserved");
        assert!(
            !result
                .locks_removed
                .iter()
                .any(|path| path.ends_with(".git/index.lock")),
            "healer must not report index.lock removal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_heal_archive_locks_skips_symlinked_project_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        ensure_archive_root(&config).unwrap();

        let outside = outside_tmp.path().join("outside-project");
        let outside_lock = outside.join("escape.lock");
        fs::create_dir_all(&outside).unwrap();
        fs::write(&outside_lock, "lock contents").unwrap();
        let linked_project = tmp.path().join("projects").join("linked");
        fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        symlink(&outside, &linked_project).unwrap();

        let result = heal_archive_locks(&config).unwrap();
        assert!(
            outside_lock.exists(),
            "healer must not remove locks through a symlinked project directory"
        );
        assert!(
            result.locks_removed.is_empty(),
            "symlinked project directories must be skipped during healing"
        );
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
    fn test_store_attachment_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "attach-proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");
        let img_path = tmp.path().join("source.png");
        create_test_png(&img_path);

        archive.root = forged_root.clone();
        let stored = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();

        let stored_rel = stored.meta.path.expect("file attachment path");
        assert!(archive.repo_root.join(stored_rel).exists());
        assert!(real_root.join("attachments").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive attachment writes"
        );
    }

    #[test]
    fn test_store_raw_attachment_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let mut archive = ensure_archive(&test_config(tmp.path()), "raw-attach-proj").unwrap();
        let real_root = archive.root.clone();
        let forged_root = tmp.path().join("outside-project");
        let file_path = tmp.path().join("source.txt");
        fs::write(&file_path, b"hello raw attachment").unwrap();

        archive.root = forged_root.clone();
        let stored = store_raw_attachment(&archive, &file_path, 1024 * 1024).unwrap();

        let stored_rel = stored.meta.path.expect("raw attachment path");
        assert!(archive.repo_root.join(stored_rel).exists());
        assert!(real_root.join("attachments").exists());
        assert!(
            !forged_root.exists(),
            "forged archive.root must not receive raw attachment writes"
        );
    }

    #[test]
    fn test_store_raw_attachment_rewrites_mismatched_existing_target() {
        let tmp = TempDir::new().unwrap();
        let archive = ensure_archive(&test_config(tmp.path()), "raw-attach-repair-proj").unwrap();
        let file_path = tmp.path().join("source.txt");
        let source_bytes = b"hello raw attachment";
        fs::write(&file_path, source_bytes).unwrap();

        let digest = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(source_bytes);
            hex::encode(hasher.finalize())
        };
        let target_path = archive
            .root
            .join("attachments")
            .join("files")
            .join(&digest[..2])
            .join(format!("{digest}.txt"));
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();
        fs::write(&target_path, b"corrupt raw attachment").unwrap();

        let stored = store_raw_attachment(&archive, &file_path, 1024 * 1024).unwrap();

        assert_eq!(fs::read(&target_path).unwrap(), source_bytes);
        assert_eq!(stored.meta.bytes, source_bytes.len());
        assert_eq!(stored.rel_paths.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_store_raw_attachment_rejects_symlinked_existing_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let archive = ensure_archive(&test_config(tmp.path()), "raw-attach-symlink-proj").unwrap();
        let file_path = tmp.path().join("source.txt");
        let source_bytes = b"hello raw attachment";
        fs::write(&file_path, source_bytes).unwrap();

        let digest = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(source_bytes);
            hex::encode(hasher.finalize())
        };
        let target_path = archive
            .root
            .join("attachments")
            .join("files")
            .join(&digest[..2])
            .join(format!("{digest}.txt"));
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();

        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &target_path).unwrap();

        let err = store_raw_attachment(&archive, &file_path, 1024 * 1024).unwrap_err();
        assert!(err.to_string().contains("must not include symlinks"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
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
    fn test_process_attachments_empty_list_does_not_require_existing_base_dir() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "proc-empty-proj").unwrap();
        let missing_base = tmp.path().join("missing-base");

        let (meta, rel_paths) =
            process_attachments(&archive, &config, &missing_base, &[], EmbedPolicy::File).unwrap();

        assert!(meta.is_empty());
        assert!(rel_paths.is_empty());
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
    fn test_process_markdown_images_deduplicates_repeated_references() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "md-repeat-proj").unwrap();

        let img_path = archive.root.join("diagram.png");
        create_test_png(&img_path);

        let body = "One ![diagram](diagram.png)\nTwo ![diagram](diagram.png)";
        let (new_body, meta, rel_paths) =
            process_markdown_images(&archive, &config, &archive.root, body, EmbedPolicy::File)
                .unwrap();

        assert_eq!(
            new_body.matches("attachments/").count(),
            2,
            "both markdown image references should still be rewritten"
        );
        assert_eq!(
            meta.len(),
            1,
            "repeated references to the same stored attachment should not duplicate metadata"
        );

        let unique_rel_paths = rel_paths
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            rel_paths.len(),
            unique_rel_paths.len(),
            "relative archive paths should be deduplicated"
        );
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

        // Second call: cache hit — skips conversion, but still returns the
        // deterministic cache artifacts so retries can commit them if a prior
        // run crashed after writing them but before staging/committing.
        let second = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        let mut first_rel_paths = first.rel_paths.clone();
        let mut second_rel_paths = second.rel_paths.clone();
        first_rel_paths.sort();
        second_rel_paths.sort();
        assert_eq!(second_rel_paths, first_rel_paths);
        assert_eq!(second.meta.sha1, first_sha1);
        assert_eq!(second.meta.width, first.meta.width);
        assert_eq!(second.meta.height, first.meta.height);
        assert_eq!(second.meta.kind, "file");
    }

    #[test]
    fn test_store_attachment_cache_hit_respects_keep_original_images_setting() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.keep_original_images = true;
        let archive = ensure_archive(&config, "cache-original-toggle-proj").unwrap();

        let img_path = archive.root.join("cached_original_toggle.png");
        create_test_png(&img_path);

        let first = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        assert!(first.meta.original_path.is_some());

        config.keep_original_images = false;
        let second = store_attachment(&archive, &config, &img_path, EmbedPolicy::File).unwrap();
        assert!(
            second.meta.original_path.is_none(),
            "cache hits must follow the current keep_original_images setting"
        );
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

        // Verify the fallback constant is the expected value.
        assert_eq!(FALLBACK_MAX_ATTACHMENT_BYTES, 50 * 1024 * 1024);

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
    fn test_find_commit_for_path_does_not_confuse_sibling_prefixes_on_root_commit() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "read-sibling-proj").unwrap();

        let agent = serde_json::json!({"name": "AB", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits();

        let missing_rel = "projects/read-sibling-proj/agents/A/profile.json".to_string();
        let commit = find_commit_for_path(&archive, &missing_rel).unwrap();
        assert!(
            commit.is_none(),
            "path lookup must not attribute sibling commits via string-prefix matching"
        );
    }

    #[test]
    fn test_find_commit_for_path_rejects_invalid_repo_relative_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "read-invalid-path-proj").unwrap();

        let err = find_commit_for_path(&archive, "../escape").expect_err("expected invalid path");
        assert!(
            err.to_string().contains("repo-relative path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_recent_commits_path_filter_includes_deletions() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "delete-filter-proj").unwrap();

        let agent = serde_json::json!({"name": "DeleteAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits();

        let profile_path = archive
            .root
            .join("agents")
            .join("DeleteAgent")
            .join("profile.json");
        let rel = rel_path_cached(&archive.canonical_repo_root, &profile_path).unwrap();

        fs::remove_file(&profile_path).unwrap();
        enqueue_async_commit(
            &archive.repo_root,
            &config,
            "delete profile",
            std::slice::from_ref(&rel),
        );
        flush_async_commits();

        let commits = get_recent_commits(&archive, 10, Some(&rel)).unwrap();
        assert!(
            commits
                .iter()
                .any(|commit| commit.summary == "delete profile"),
            "path-filtered history must include deletion commits that only touch old_file paths"
        );
    }

    #[test]
    fn test_get_recent_commits_rejects_invalid_path_filter() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "invalid-filter-proj").unwrap();

        let err =
            get_recent_commits(&archive, 10, Some("../escape")).expect_err("expected invalid path");
        assert!(
            err.to_string().contains("path filter"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_recent_commits_rejects_symlinked_archive_repo_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "recent-symlinked-repo-root").unwrap();
        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();
        archive.repo_root = linked_root;

        let err = get_recent_commits(&archive, 10, None)
            .expect_err("expected symlinked archive repo root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_find_commit_for_path_rejects_symlinked_archive_repo_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "find-symlinked-repo-root").unwrap();
        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();
        archive.repo_root = linked_root;

        let err = find_commit_for_path(&archive, "project.json")
            .expect_err("expected symlinked archive repo root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_get_commits_by_author_rejects_symlinked_archive_repo_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "author-symlinked-repo-root").unwrap();
        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();
        archive.repo_root = linked_root;

        let err = get_commits_by_author(&archive, &config.git_author_name, 10)
            .expect_err("expected symlinked archive repo root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
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
        assert_eq!(body, "This is the body.\n");
    }

    #[test]
    fn test_read_message_file_preserves_body_whitespace() {
        let tmp = TempDir::new().unwrap();

        let msg_path = tmp.path().join("whitespace_msg.md");
        let content = "---json\n{\"id\": 2}\n---\n\n  leading\nbody line\n\ntrailing  ";
        fs::write(&msg_path, content).unwrap();

        let (frontmatter, body) = read_message_file(&msg_path).unwrap();
        assert_eq!(frontmatter["id"], 2);
        assert_eq!(body, "  leading\nbody line\n\ntrailing  ");
    }

    #[test]
    fn test_read_message_file_preserves_leading_blank_lines_in_body() {
        let tmp = TempDir::new().unwrap();

        let msg_path = tmp.path().join("leading_blank_lines.md");
        let content = "---json\n{\"id\": 3}\n---\n\n\n\nbody after blanks";
        fs::write(&msg_path, content).unwrap();

        let (frontmatter, body) = read_message_file(&msg_path).unwrap();
        assert_eq!(frontmatter["id"], 3);
        assert_eq!(body, "\n\nbody after blanks");
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

    #[cfg(unix)]
    #[test]
    fn test_read_message_file_rejects_symlinked_path() {
        use std::os::unix::fs as unix_fs;

        let outside = TempDir::new().unwrap();
        let outside_path = outside.path().join("outside.md");
        fs::write(&outside_path, "outside").unwrap();

        let tmp = TempDir::new().unwrap();
        let msg_path = tmp.path().join("linked.md");
        unix_fs::symlink(&outside_path, &msg_path).unwrap();

        let err = read_message_file(&msg_path).expect_err("expected symlink rejection");
        assert!(
            err.to_string().contains("symlinked path"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_read_message_file_rejects_symlinked_parent_directory() {
        use std::os::unix::fs as unix_fs;

        let outside = TempDir::new().unwrap();
        let outside_dir = outside.path().join("messages");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_path = outside_dir.join("outside.md");
        fs::write(&outside_path, "outside").unwrap();

        let tmp = TempDir::new().unwrap();
        let linked_dir = tmp.path().join("linked");
        unix_fs::symlink(&outside_dir, &linked_dir).unwrap();
        let msg_path = linked_dir.join("outside.md");

        let err = read_message_file(&msg_path).expect_err("expected symlink-parent rejection");
        assert!(
            err.to_string().contains("symlinked path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_historical_inbox_snapshot_returns_message_metadata() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "snapshot-proj").unwrap();

        let message = serde_json::json!({
            "id": 42,
            "from": "SenderAgent",
            "subject": "Snapshot Subject",
            "importance": "high",
            "created_ts": "2026-01-20T12:00:00Z",
            "thread_id": "SNAP-1",
        });
        write_message_bundle(
            &archive,
            &config,
            &message,
            "snapshot body",
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();
        flush_async_commits();

        let snapshot =
            get_historical_inbox_snapshot(&archive, "RecipientAgent", "2100-01-01T00:00", 200)
                .unwrap();
        let messages = snapshot
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["subject"], "Snapshot Subject");
        assert_eq!(messages[0]["from"], "SenderAgent");
        assert_eq!(messages[0]["importance"], "high");
        assert!(snapshot["snapshot_time"].is_string());
        assert!(snapshot["commit_sha"].is_string());
        assert_eq!(snapshot["requested_time"], "2100-01-01T00:00");
    }

    #[test]
    fn test_get_historical_inbox_snapshot_invalid_timestamp_returns_error_payload() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "snapshot-invalid-ts").unwrap();

        let snapshot =
            get_historical_inbox_snapshot(&archive, "RecipientAgent", "not-a-timestamp", 200)
                .unwrap();
        assert_eq!(snapshot["messages"], serde_json::json!([]));
        assert!(snapshot["snapshot_time"].is_null());
        assert!(snapshot["commit_sha"].is_null());
        assert_eq!(snapshot["requested_time"], "not-a-timestamp");
        assert_eq!(snapshot["error"], "Invalid timestamp format");
    }

    #[test]
    fn test_get_archive_tree_rejects_invalid_archive_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "tree-invalid-slug").unwrap();
        archive.slug = "../escape".to_string();

        let err = get_archive_tree(&archive, "").expect_err("expected invalid slug");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_archive_tree_rejects_symlinked_archive_repo_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "tree-symlinked-repo-root").unwrap();
        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();
        archive.repo_root = linked_root;

        let err =
            get_archive_tree(&archive, "").expect_err("expected symlinked repo root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_archive_file_content_rejects_invalid_archive_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "file-invalid-slug").unwrap();
        archive.slug = "../escape".to_string();

        let err = get_archive_file_content(&archive, "project.json", 4096)
            .expect_err("expected invalid slug");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_historical_inbox_snapshot_rejects_invalid_archive_slug() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "snapshot-invalid-slug").unwrap();
        archive.slug = "../escape".to_string();

        let err = get_historical_inbox_snapshot(&archive, "RecipientAgent", "2100-01-01T00:00", 5)
            .expect_err("expected invalid slug");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_communication_graph_rejects_invalid_project_slug() {
        let tmp = TempDir::new().unwrap();
        let err = get_communication_graph(tmp.path(), "../escape", 10)
            .expect_err("expected invalid slug");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_timeline_commits_rejects_invalid_project_slug() {
        let tmp = TempDir::new().unwrap();
        let err =
            get_timeline_commits(tmp.path(), "../escape", 10).expect_err("expected invalid slug");
        assert!(
            err.to_string().contains("project slug"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_recent_commits_extended_rejects_symlinked_archive_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let _archive = ensure_archive(&config, "extended-symlink-root").unwrap();
        flush_async_commits();

        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();

        let err = get_recent_commits_extended(&linked_root, 10)
            .expect_err("expected symlinked archive root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_commit_detail_rejects_symlinked_archive_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "commit-detail-symlink-root").unwrap();
        let agent = serde_json::json!({"name": "CommitDetailAgent", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &agent).unwrap();
        flush_async_commits();

        let repo = Repository::open(tmp.path()).unwrap();
        let head_sha = repo.head().unwrap().target().unwrap().to_string();

        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();

        let err = get_commit_detail(&linked_root, &head_sha, 4096)
            .expect_err("expected symlinked archive root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_communication_graph_rejects_symlinked_archive_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "graph-symlink-root").unwrap();
        let message = serde_json::json!({
            "id": 12,
            "subject": "Graph test",
            "created_ts": "2026-01-20T12:00:00Z",
        });
        write_message_bundle(
            &archive,
            &config,
            &message,
            "body",
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();
        flush_async_commits();

        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();

        let err = get_communication_graph(&linked_root, "graph-symlink-root", 10)
            .expect_err("expected symlinked archive root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_timeline_commits_rejects_symlinked_archive_root() {
        use std::os::unix::fs as unix_fs;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "timeline-symlink-root").unwrap();
        let message = serde_json::json!({
            "id": 34,
            "subject": "Timeline test",
            "created_ts": "2026-01-20T12:00:00Z",
        });
        write_message_bundle(
            &archive,
            &config,
            &message,
            "body",
            "SenderAgent",
            &["RecipientAgent".to_string()],
            &[],
            None,
        )
        .unwrap();
        flush_async_commits();

        let linked_root = tmp.path().join("linked-root");
        unix_fs::symlink(tmp.path(), &linked_root).unwrap();

        let err = get_timeline_commits(&linked_root, "timeline-symlink-root", 10)
            .expect_err("expected symlinked archive root rejection");
        assert!(
            err.to_string().contains("archive root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_get_historical_inbox_snapshot_limit_returns_most_recent_messages() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "snapshot-limit-order").unwrap();

        for (id, subject, created_ts) in [
            (1, "Oldest", "2026-01-20T12:00:00Z"),
            (2, "Middle", "2026-01-20T12:01:00Z"),
            (3, "Newest", "2026-01-20T12:02:00Z"),
        ] {
            let message = serde_json::json!({
                "id": id,
                "from": "SenderAgent",
                "subject": subject,
                "importance": "normal",
                "created_ts": created_ts,
                "thread_id": "SNAP-LIMIT",
            });
            write_message_bundle(
                &archive,
                &config,
                &message,
                "snapshot body",
                "SenderAgent",
                &["RecipientAgent".to_string()],
                &[],
                None,
            )
            .unwrap();
        }
        flush_async_commits();

        let snapshot =
            get_historical_inbox_snapshot(&archive, "RecipientAgent", "2100-01-01T00:00", 2)
                .unwrap();
        let messages = snapshot
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["subject"], "Newest");
        assert_eq!(messages[1]["subject"], "Middle");
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

    #[cfg(unix)]
    #[test]
    fn test_list_agent_inbox_skips_symlinked_inbox_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "list-proj").unwrap();

        let outside_inbox = tmp.path().join("outside-inbox").join("2026").join("01");
        fs::create_dir_all(&outside_inbox).unwrap();
        fs::write(outside_inbox.join("escape.md"), "outside message").unwrap();

        let agent_dir = archive.root.join("agents").join("Recipient");
        fs::create_dir_all(&agent_dir).unwrap();
        symlink(
            outside_inbox.parent().unwrap().parent().unwrap(),
            agent_dir.join("inbox"),
        )
        .unwrap();

        let inbox = list_agent_inbox(&archive, "Recipient").unwrap();
        assert!(
            inbox.is_empty(),
            "symlinked inbox directories must not be traversed"
        );
    }

    #[test]
    fn test_list_agent_inbox_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "list-proj").unwrap();

        let outside_root = tmp.path().join("outside-project");
        let outside_inbox = outside_root
            .join("agents")
            .join("Recipient")
            .join("inbox")
            .join("2026")
            .join("01");
        fs::create_dir_all(&outside_inbox).unwrap();
        fs::write(outside_inbox.join("escape.md"), "outside message").unwrap();

        archive.root = outside_root;
        let inbox = list_agent_inbox(&archive, "Recipient").unwrap();
        assert!(
            inbox.is_empty(),
            "forged archive.root must not redirect inbox reads"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_list_archive_agents_skips_symlinked_entries() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "agents-proj").unwrap();

        let real_agent = serde_json::json!({"name": "Alice", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &real_agent).unwrap();

        let outside_agent = tmp.path().join("outside-agent");
        fs::create_dir_all(&outside_agent).unwrap();
        fs::write(
            outside_agent.join("profile.json"),
            serde_json::json!({"name": "Ghost", "program": "outside"}).to_string(),
        )
        .unwrap();
        symlink(&outside_agent, archive.root.join("agents").join("Ghost")).unwrap();

        let linked_profile_dir = archive.root.join("agents").join("LinkedProfile");
        fs::create_dir_all(&linked_profile_dir).unwrap();
        symlink(
            outside_agent.join("profile.json"),
            linked_profile_dir.join("profile.json"),
        )
        .unwrap();

        let agents = list_archive_agents(&archive).unwrap();
        assert_eq!(
            agents,
            vec!["Alice"],
            "symlinked agent directories and profile files must be ignored"
        );
    }

    #[test]
    fn test_list_archive_agents_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "agents-proj").unwrap();

        let outside_root = tmp.path().join("outside-project");
        let outside_agent = outside_root.join("agents").join("Ghost");
        fs::create_dir_all(&outside_agent).unwrap();
        fs::write(
            outside_agent.join("profile.json"),
            serde_json::json!({"name": "Ghost", "program": "outside"}).to_string(),
        )
        .unwrap();

        archive.root = outside_root;
        let agents = list_archive_agents(&archive).unwrap();
        assert!(
            agents.is_empty(),
            "forged archive.root must not redirect agent enumeration"
        );
    }

    #[test]
    fn test_list_archive_agents_ignores_forged_archive_repo_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "agents-proj").unwrap();

        let real_agent = serde_json::json!({"name": "Alice", "program": "test"});
        write_agent_profile_with_config(&archive, &config, &real_agent).unwrap();

        let forged_repo_root = tmp.path().join("outside-repo");
        let outside_agent = forged_repo_root
            .join("projects")
            .join("agents-proj")
            .join("agents")
            .join("Ghost");
        fs::create_dir_all(&outside_agent).unwrap();
        fs::write(
            outside_agent.join("profile.json"),
            serde_json::json!({"name": "Ghost", "program": "outside"}).to_string(),
        )
        .unwrap();

        archive.repo_root = forged_repo_root;
        let agents = list_archive_agents(&archive).unwrap();
        assert_eq!(
            agents,
            vec!["Alice"],
            "forged archive.repo_root must not redirect agent enumeration"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_read_agent_profile_returns_none_for_symlinked_profile() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "profile-proj").unwrap();

        let outside_profile = tmp.path().join("outside-profile.json");
        fs::write(
            &outside_profile,
            serde_json::json!({"name": "Ghost", "program": "outside"}).to_string(),
        )
        .unwrap();

        let linked_profile_dir = archive.root.join("agents").join("Ghost");
        fs::create_dir_all(&linked_profile_dir).unwrap();
        symlink(&outside_profile, linked_profile_dir.join("profile.json")).unwrap();

        let profile = read_agent_profile(&archive, "Ghost").unwrap();
        assert!(
            profile.is_none(),
            "symlinked profile files must not be read"
        );
    }

    #[test]
    fn test_read_agent_profile_ignores_forged_archive_root() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut archive = ensure_archive(&config, "profile-proj").unwrap();

        let outside_root = tmp.path().join("outside-project");
        let outside_profile_dir = outside_root.join("agents").join("Ghost");
        fs::create_dir_all(&outside_profile_dir).unwrap();
        fs::write(
            outside_profile_dir.join("profile.json"),
            serde_json::json!({"name": "Ghost", "program": "outside"}).to_string(),
        )
        .unwrap();

        archive.root = outside_root;
        let profile = read_agent_profile(&archive, "Ghost").unwrap();
        assert!(
            profile.is_none(),
            "forged archive.root must not redirect profile reads"
        );
    }

    // -----------------------------------------------------------------------
    // Write-Behind Queue (WBQ) tests
    // -----------------------------------------------------------------------

    fn wbq_test_clear_signal_op(project_slug: &str) -> WriteOp {
        WriteOp::ClearSignal {
            config: Config::default(),
            project_slug: project_slug.to_string(),
            agent_name: "WbqTestAgent".to_string(),
        }
    }

    #[test]
    fn wbq_enqueue_returns_true_when_running() {
        wbq_start();
        let op = wbq_test_clear_signal_op("test-wbq");
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
        let op = wbq_test_clear_signal_op("test-stats");
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
        let op = wbq_test_clear_signal_op("test-flush");
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

        enqueue_with_retry(op, "wbq_message_bundle_roundtrip");
        wbq_flush();
        flush_async_commits();

        // Verify message files exist
        let archive = ensure_archive(&config, "wbq-msg-test").unwrap();
        let messages_dir = archive.root.join("messages");
        assert!(messages_dir.exists(), "messages/ directory should exist");
    }

    #[test]
    fn wbq_drain_loop_message_bundle_burst_uses_batch_archive_commit() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let project_slug = "wbq-msg-batch-test";
        let _archive = ensure_archive(&config, project_slug).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);

        for id in [41_i64, 42] {
            let msg_json = serde_json::json!({
                "id": id,
                "from": "Sender",
                "to": ["Receiver"],
                "subject": format!("WBQ batch test {id}"),
                "created": "2025-01-01T00:00:00+00:00",
                "thread_id": "WBQ-BATCH",
                "importance": "normal",
            });

            let op = WriteOp::MessageBundle {
                project_slug: project_slug.to_string(),
                config: config.clone(),
                message_json: msg_json,
                body_md: format!("Hello from WBQ batch message {id}"),
                sender: "Sender".to_string(),
                recipients: vec!["Receiver".to_string()],
                extra_paths: vec![],
            };

            tx.send(WbqMsg::Op(WbqOpEnvelope {
                enqueued_at: Instant::now(),
                op: Box::new(op),
            }))
            .unwrap();
        }
        tx.send(WbqMsg::Shutdown).unwrap();

        wbq_drain_loop(rx, Arc::new(AtomicU64::new(2)), 4, 4);
        flush_async_commits();

        let archive = ensure_archive(&config, project_slug).unwrap();
        let commits = get_recent_commits(&archive, 3, None).unwrap();
        assert_eq!(commits[0].summary, "batch: 2 message bundles");
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

        enqueue_with_retry(op, "wbq_file_reservation_roundtrip");
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

        enqueue_with_retry(op, "wbq_notification_signal_roundtrip");
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
    fn wbq_enqueue_with_sender_disconnected_receiver_returns_queue_unavailable() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        drop(rx); // simulate drain worker death/panic (receiver dropped)
        let op_depth = AtomicU64::new(0);

        let result = wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-disc"));
        assert_eq!(result, WbqEnqueueResult::QueueUnavailable);
        assert_eq!(op_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn wbq_enqueue_with_sender_success_path_increments_depth() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let op_depth = AtomicU64::new(0);

        let result = wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-ok"));
        assert_eq!(result, WbqEnqueueResult::Enqueued);
        assert_eq!(op_depth.load(Ordering::Relaxed), 1);
        let _ = rx.recv_timeout(Duration::from_millis(20));
    }

    #[test]
    fn wbq_enqueue_with_sender_times_out_when_channel_stays_full() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(wbq_test_clear_signal_op("wbq-prefill")),
        }))
        .expect("prefill should succeed");

        let op_depth = AtomicU64::new(0);
        let before = wbq_stats();
        let result = wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-full"));
        let after = wbq_stats();

        assert_eq!(result, WbqEnqueueResult::QueueUnavailable);
        assert_eq!(
            op_depth.load(Ordering::Relaxed),
            0,
            "timed-out enqueue must not count as enqueued"
        );
        assert!(
            after.fallbacks >= before.fallbacks.saturating_add(1),
            "full queue path should increment fallback counter"
        );
        drop(rx);
    }

    #[test]
    fn wbq_enqueue_with_sender_recovers_when_backpressure_clears_before_deadline() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(wbq_test_clear_signal_op("wbq-prefill-recover")),
        }))
        .expect("prefill should succeed");

        let drain = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let _ = rx.recv_timeout(Duration::from_millis(100));
            let _ = rx.recv_timeout(Duration::from_millis(100));
        });

        let op_depth = AtomicU64::new(0);
        let result =
            wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-recover"));

        drain.join().expect("drain helper should not panic");
        assert_eq!(result, WbqEnqueueResult::Enqueued);
        assert_eq!(op_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn wbq_enqueue_with_sender_timeout_preserves_prefilled_item() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(wbq_test_clear_signal_op("wbq-prefill-preserve")),
        }))
        .expect("prefill should succeed");

        let op_depth = AtomicU64::new(0);
        let result =
            wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-drop-on-full"));
        assert_eq!(result, WbqEnqueueResult::QueueUnavailable);

        let preserved = rx.recv_timeout(Duration::from_millis(20));
        assert!(preserved.is_ok(), "prefilled item should still be readable");
    }

    #[test]
    fn wbq_enqueue_with_sender_full_then_disconnected_still_returns_queue_unavailable() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(wbq_test_clear_signal_op("wbq-prefill-disc")),
        }))
        .expect("prefill should succeed");

        let disconnect = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drop(rx);
        });

        let op_depth = AtomicU64::new(0);
        let result =
            wbq_enqueue_with_sender(&tx, &op_depth, wbq_test_clear_signal_op("wbq-full-disc"));
        disconnect
            .join()
            .expect("disconnect helper should not panic");

        assert_eq!(result, WbqEnqueueResult::QueueUnavailable);
        assert_eq!(op_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn wbq_enqueue_skips_under_critical_disk_pressure() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let op_depth = AtomicU64::new(0);
        let result = wbq_enqueue_with_sender_and_pressure(
            &tx,
            &op_depth,
            wbq_test_clear_signal_op("wbq-disk-critical"),
            mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64(),
        );
        assert_eq!(result, WbqEnqueueResult::SkippedDiskCritical);
        assert_eq!(op_depth.load(Ordering::Relaxed), 0);
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn wbq_enqueue_recovers_after_disk_pressure_clears() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let op_depth = AtomicU64::new(0);

        let skipped = wbq_enqueue_with_sender_and_pressure(
            &tx,
            &op_depth,
            wbq_test_clear_signal_op("wbq-disk-recover"),
            mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64(),
        );
        assert_eq!(skipped, WbqEnqueueResult::SkippedDiskCritical);

        let accepted = wbq_enqueue_with_sender_and_pressure(
            &tx,
            &op_depth,
            wbq_test_clear_signal_op("wbq-disk-recover"),
            0,
        );
        assert_eq!(accepted, WbqEnqueueResult::Enqueued);
        assert_eq!(op_depth.load(Ordering::Relaxed), 1);
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_ok());
    }

    struct DiskPressureReset(u64);

    impl DiskPressureReset {
        fn set(level: u64) -> Self {
            let metrics = mcp_agent_mail_core::global_metrics();
            let previous = metrics.system.disk_pressure_level.load();
            metrics.system.disk_pressure_level.set(level);
            Self(previous)
        }
    }

    impl Drop for DiskPressureReset {
        fn drop(&mut self) {
            mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .set(self.0);
        }
    }

    #[test]
    fn wbq_drain_executes_already_enqueued_ops_even_if_disk_turns_critical() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let project_slug = "wbq-drain-critical".to_string();

        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let op_depth = Arc::new(AtomicU64::new(1));
        let agent_json = serde_json::json!({
            "name": "DrainCriticalAgent",
            "program": "wbq-test",
            "model": "gpt5",
        });
        tx.send(WbqMsg::Op(WbqOpEnvelope {
            enqueued_at: Instant::now(),
            op: Box::new(WriteOp::AgentProfile {
                project_slug: project_slug.clone(),
                config: config.clone(),
                agent_json,
            }),
        }))
        .unwrap();
        tx.send(WbqMsg::Shutdown).unwrap();
        drop(tx);

        let _pressure =
            DiskPressureReset::set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
        wbq_drain_loop(rx, op_depth, 4, 4);
        flush_async_commits();

        let archive = ensure_archive(&config, &project_slug).unwrap();
        let profile_path = archive
            .root
            .join("agents")
            .join("DrainCriticalAgent")
            .join("profile.json");
        assert!(
            profile_path.exists(),
            "drain must not drop already-enqueued archive writes when disk pressure changes later"
        );
    }

    #[test]
    fn wbq_burst_10k_ops_drains_without_data_loss() {
        wbq_start();

        let before = wbq_stats();
        let mut enqueued_count = 0_u64;

        for i in 0..10_000_u64 {
            let op = WriteOp::ClearSignal {
                config: Config::default(),
                project_slug: format!("wbq-10k-{i}"),
                agent_name: "Burst10kAgent".to_string(),
            };
            if wbq_enqueue(op) == WbqEnqueueResult::Enqueued {
                enqueued_count += 1;
            }
        }

        wbq_flush();

        let after = wbq_stats();
        let enqueued_delta = after.enqueued.saturating_sub(before.enqueued);
        let drained_delta = after.drained.saturating_sub(before.drained);

        assert!(
            enqueued_delta >= enqueued_count,
            "expected enqueued_delta >= enqueued_count ({enqueued_delta} >= {enqueued_count})"
        );
        assert!(
            drained_delta >= enqueued_count,
            "expected drained_delta >= enqueued_count ({drained_delta} >= {enqueued_count})"
        );
    }

    #[test]
    fn wbq_burst_100_profile_writes_batches_git_commits() {
        wbq_start();
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let project_slug = "wbq-batch-100".to_string();
        let archive = ensure_archive(&config, &project_slug).unwrap();
        let repo_root = archive.repo_root.clone();

        let coalescer = get_commit_coalescer();
        let stats_before = coalescer
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();

        for i in 0..100_u64 {
            let agent_json = serde_json::json!({
                "name": format!("BatchAgent{i}"),
                "program": "wbq-batch-test",
                "model": "gpt5",
            });
            let op = WriteOp::AgentProfile {
                project_slug: project_slug.clone(),
                config: config.clone(),
                agent_json,
            };
            assert_eq!(wbq_enqueue(op), WbqEnqueueResult::Enqueued);
        }

        wbq_flush();
        flush_async_commits();

        let stats_after = coalescer
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();
        let enqueued_delta = stats_after
            .enqueued_total
            .saturating_sub(stats_before.enqueued_total);
        let commits_delta = stats_after
            .commits_total
            .saturating_sub(stats_before.commits_total);
        assert!(
            enqueued_delta >= 100,
            "expected repo-local enqueue delta >= 100, got {enqueued_delta}"
        );
        assert!(
            commits_delta > 0,
            "burst should produce at least one coalesced commit"
        );
        assert!(
            commits_delta < 100,
            "coalescing should reduce commit count below enqueue count, got {commits_delta}"
        );
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

    #[cfg(unix)]
    #[test]
    fn collect_lock_status_skips_symlinked_lock_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());

        let outside = outside_tmp.path().join("outside-locks");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("escape.lock"), "lock").unwrap();
        let linked_project = tmp.path().join("projects").join("linked");
        fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        symlink(&outside, &linked_project).unwrap();

        let result = collect_lock_status(&config).unwrap();
        let locks = result["locks"].as_array().unwrap();
        assert!(
            locks.is_empty(),
            "symlinked directories must not contribute lock diagnostics"
        );
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
        // {root}/projects/{slug}/messages/2026/02/{iso}__hello-world__42.md
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
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
    fn consistency_same_second_wrong_subject_stays_missing() {
        let dir = tempfile::tempdir().unwrap();
        let slug = "test-project";
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
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
                subject: "different subject".into(),
                created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
            },
        ];

        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 2);
        assert_eq!(report.found, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![99]);
    }

    #[test]
    fn consistency_same_second_id_drift_with_matching_frontmatter_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let slug = "test-project";
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();

        let message = serde_json::json!({
            "id": 42,
            "from": "BlueLake",
            "subject": "hello world",
            "created": "2026-02-08T03:29:30+00:00",
            "to": ["RedFox"],
        });
        let content = render_message_bundle_content(&message, "body").unwrap();
        fs::write(
            msg_dir.join("2026-02-08T03-29-30+00-00__hello-world__42.md"),
            content,
        )
        .unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: slug.into(),
            message_id: 99,
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
    fn consistency_same_second_same_subject_different_sender_stays_missing() {
        let dir = tempfile::tempdir().unwrap();
        let slug = "test-project";
        let msg_dir = dir
            .path()
            .join("projects")
            .join(slug)
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();

        let message = serde_json::json!({
            "id": 42,
            "from": "BlueLake",
            "subject": "hello world",
            "created": "2026-02-08T03:29:30+00:00",
            "to": ["RedFox"],
        });
        let content = render_message_bundle_content(&message, "body").unwrap();
        fs::write(
            msg_dir.join("2026-02-08T03-29-30+00-00__hello-world__42.md"),
            content,
        )
        .unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: slug.into(),
            message_id: 99,
            sender_name: "RedFox".into(),
            subject: "hello world".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];

        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
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
        let max_workers = Config::get().coalescer_max_workers;
        let min_workers = if max_workers <= 1 { 1 } else { 2 };
        let wc = coalescer_worker_count();
        assert!(
            wc >= min_workers,
            "worker count should be >= {min_workers}, got {wc}"
        );
        assert!(
            wc <= max_workers,
            "worker count should be <= configured max {max_workers}, got {wc}"
        );
    }

    #[test]
    fn coalescer_worker_count_honors_single_worker_override() {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_COALESCER_MAX_WORKERS", "1")],
            || {
                assert_eq!(coalescer_worker_count(), 1);
            },
        );
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

    #[test]
    fn coalescer_normalizes_repo_root_identity() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("archive");
        let repo_root_alias = repo_root.join("alias").join("..");

        let mut config_a = test_config(tmp.path());
        config_a.storage_root = repo_root.clone();
        fs::create_dir_all(repo_root.join("alias")).unwrap();
        let mut config_b = config_a.clone();
        config_b.storage_root = repo_root_alias;

        let archive_a = ensure_archive(&config_a, "canon-a").unwrap();
        let archive_b = ensure_archive(&config_b, "canon-b").unwrap();

        assert_ne!(
            archive_a.repo_root.as_os_str(),
            archive_b.repo_root.as_os_str(),
            "test setup requires distinct textual repo roots"
        );
        assert_eq!(
            archive_a.canonical_repo_root, archive_b.canonical_repo_root,
            "test setup requires both paths to resolve to the same repo"
        );

        let agent = serde_json::json!({"name": "CanonAgent", "program": "test"});
        write_agent_profile_with_config(&archive_a, &config_a, &agent).unwrap();
        write_agent_profile_with_config(&archive_b, &config_b, &agent).unwrap();

        flush_async_commits();

        let per_repo = get_commit_coalescer().per_repo_stats();
        assert!(
            per_repo.contains_key(&archive_a.canonical_repo_root),
            "canonical repo root should be the coalescer key"
        );
        assert!(
            !per_repo.contains_key(&archive_b.repo_root),
            "non-canonical repo-root aliases should not survive as separate queue keys"
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
        let repo_root = archive.repo_root.clone();

        let coalescer = get_commit_coalescer();
        let stats_before = coalescer
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();

        let burst_count = 100usize;
        let burst_count_u64 = u64::try_from(burst_count).unwrap_or(u64::MAX);
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

        let stats_after = coalescer
            .per_repo_stats()
            .get(&repo_root)
            .cloned()
            .unwrap_or_default();
        let enqueued_delta = stats_after
            .enqueued_total
            .saturating_sub(stats_before.enqueued_total);
        let commits_delta = stats_after
            .commits_total
            .saturating_sub(stats_before.commits_total);

        assert!(
            enqueued_delta >= burst_count_u64,
            "expected at least {burst_count} enqueued, got delta {enqueued_delta}"
        );

        // Batching should reduce commit count significantly.
        if commits_delta > 0 {
            assert!(
                commits_delta < burst_count_u64 / 2,
                "batching should reduce commits: {commits_delta} commits for {enqueued_delta} \
                 enqueues (expected < {}, avg batch = {:.1})",
                burst_count / 2,
                enqueued_delta as f64 / commits_delta as f64,
            );
        }
    }

    #[test]
    fn coalescer_commit_batch_returns_failed_requests_on_commit_error() {
        let tmp = TempDir::new().unwrap();
        let stats = std::sync::Arc::new(std::sync::Mutex::new(CommitQueueStats::default()));
        let batch_sizes =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let request = CoalescerCommitFields {
            enqueued_at: std::time::Instant::now(),
            enqueued_wall: Utc::now(),
            git_author_name: "Test".to_string(),
            git_author_email: "test@example.com".to_string(),
            message: "broken commit".to_string(),
            rel_paths: vec!["missing.txt".to_string()],
        };

        let outcome = coalescer_commit_batch(
            tmp.path(),
            std::slice::from_ref(&request),
            &stats,
            &batch_sizes,
        );

        assert_eq!(outcome.committed_requests, 0);
        assert_eq!(outcome.committed_commits, 0);
        assert_eq!(outcome.failed_requests.len(), 1);
        assert_eq!(outcome.failed_requests[0].message, request.message);
        assert_eq!(stats.lock().unwrap_or_else(|e| e.into_inner()).errors, 1);
    }

    #[test]
    fn coalescer_commit_batch_tracks_partial_sequential_success_counts() {
        let tmp = TempDir::new().unwrap();
        Repository::init(tmp.path()).expect("init repository");
        std::fs::write(tmp.path().join("ok.txt"), "ok\n").expect("write tracked file");

        let stats = std::sync::Arc::new(std::sync::Mutex::new(CommitQueueStats::default()));
        let batch_sizes =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let ok_request = CoalescerCommitFields {
            enqueued_at: std::time::Instant::now(),
            enqueued_wall: Utc::now(),
            git_author_name: "Test".to_string(),
            git_author_email: "test@example.com".to_string(),
            message: "good commit".to_string(),
            rel_paths: vec!["ok.txt".to_string()],
        };
        let failing_request = CoalescerCommitFields {
            enqueued_at: std::time::Instant::now(),
            enqueued_wall: Utc::now(),
            git_author_name: "Test".to_string(),
            git_author_email: "test@example.com".to_string(),
            message: "broken commit".to_string(),
            rel_paths: vec!["ok.txt".to_string(), "../missing.txt".to_string()],
        };

        let outcome = coalescer_commit_batch(
            tmp.path(),
            &[ok_request.clone(), failing_request.clone()],
            &stats,
            &batch_sizes,
        );

        assert_eq!(outcome.committed_requests, 1);
        assert_eq!(outcome.committed_commits, 1);
        assert_eq!(outcome.failed_requests.len(), 1);
        assert_eq!(outcome.failed_requests[0].message, failing_request.message);

        let stats = stats.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(stats.commits, 1);
        assert_eq!(stats.batched, 1);
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn coalescer_restore_spilled_work_restores_depth_and_paths() {
        let rq = RepoQueue {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            spill: std::sync::Mutex::new(CoalescerSpillState::default()),
            depth: AtomicU64::new(0),
            processing: AtomicBool::new(false),
            last_serviced_us: AtomicU64::new(0),
            metrics: RepoCommitMetrics::default(),
        };
        let work = CoalescerSpilledWork {
            repo_root: std::path::PathBuf::from("/tmp/fake-repo"),
            pending_requests: 2,
            earliest_enqueued_at: std::time::Instant::now(),
            earliest_enqueued_wall: Utc::now(),
            latest_enqueued_wall: Utc::now(),
            dirty_all: false,
            paths: vec!["a.txt".to_string(), "b.txt".to_string()],
            git_author_name: "Spill".to_string(),
            git_author_email: "spill@example.com".to_string(),
            message_first_lines: vec!["first".to_string()],
            message_total: 2,
        };

        coalescer_restore_spilled_work(&rq, work);

        assert_eq!(rq.depth.load(Ordering::Relaxed), 2);
        let spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        let restored = spill.inner.as_ref().expect("spill should be restored");
        assert_eq!(restored.pending_requests, 2);
        assert!(restored.paths.contains("a.txt"));
        assert!(restored.paths.contains("b.txt"));
        assert_eq!(restored.message_total, 2);
    }

    #[test]
    fn spill_depth_roundtrip_tracks_spilled_requests_without_underflow() {
        let rq = RepoQueue {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            spill: std::sync::Mutex::new(CoalescerSpillState::default()),
            depth: AtomicU64::new(0),
            processing: AtomicBool::new(false),
            last_serviced_us: AtomicU64::new(0),
            metrics: RepoCommitMetrics::default(),
        };

        CommitCoalescer::spill_to_repo(
            &rq,
            CoalescerCommitFields {
                enqueued_at: std::time::Instant::now(),
                enqueued_wall: Utc::now(),
                git_author_name: "Spill".to_string(),
                git_author_email: "spill@example.com".to_string(),
                message: "first".to_string(),
                rel_paths: vec!["a.txt".to_string()],
            },
        );
        CommitCoalescer::spill_to_repo(
            &rq,
            CoalescerCommitFields {
                enqueued_at: std::time::Instant::now(),
                enqueued_wall: Utc::now(),
                git_author_name: "Spill".to_string(),
                git_author_email: "spill@example.com".to_string(),
                message: "second".to_string(),
                rel_paths: vec!["b.txt".to_string()],
            },
        );

        assert_eq!(rq.depth.load(Ordering::Relaxed), 2);

        let drained =
            coalescer_drain_repo_spill(&rq, std::path::Path::new("/tmp/fake-repo")).expect("spill");
        assert_eq!(drained.pending_requests, 2);
        assert_eq!(rq.depth.load(Ordering::Relaxed), 0);

        coalescer_restore_spilled_work(&rq, drained);
        assert_eq!(rq.depth.load(Ordering::Relaxed), 2);
        let spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            spill.inner.as_ref().map(|repo| repo.pending_requests),
            Some(2)
        );
    }

    #[test]
    fn coalescer_restore_drained_work_on_panic_requeues_inflight_before_remaining_batch() {
        let rq = RepoQueue {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            spill: std::sync::Mutex::new(CoalescerSpillState::default()),
            depth: AtomicU64::new(0),
            processing: AtomicBool::new(false),
            last_serviced_us: AtomicU64::new(0),
            metrics: RepoCommitMetrics::default(),
        };
        let mk_req = |message: &str, path: &str| CoalescerCommitFields {
            enqueued_at: std::time::Instant::now(),
            enqueued_wall: Utc::now(),
            git_author_name: "Panic".to_string(),
            git_author_email: "panic@example.com".to_string(),
            message: message.to_string(),
            rel_paths: vec![path.to_string()],
        };
        let mut pending_batch = vec![mk_req("third", "c.txt"), mk_req("fourth", "d.txt")];
        let mut inflight_batch = Some(vec![mk_req("first", "a.txt"), mk_req("second", "b.txt")]);
        let mut pending_spilled = Some(CoalescerSpilledWork {
            repo_root: std::path::PathBuf::from("/tmp/fake-repo"),
            pending_requests: 2,
            earliest_enqueued_at: std::time::Instant::now(),
            earliest_enqueued_wall: Utc::now(),
            latest_enqueued_wall: Utc::now(),
            dirty_all: false,
            paths: vec!["spill-a.txt".to_string(), "spill-b.txt".to_string()],
            git_author_name: "Spill".to_string(),
            git_author_email: "spill@example.com".to_string(),
            message_first_lines: vec!["spill".to_string()],
            message_total: 2,
        });
        let mut inflight_spilled = None;

        assert!(coalescer_restore_drained_work_on_panic(
            &rq,
            &mut pending_batch,
            &mut inflight_batch,
            &mut pending_spilled,
            &mut inflight_spilled,
        ));

        assert!(pending_batch.is_empty());
        assert!(inflight_batch.is_none());
        assert!(pending_spilled.is_none());
        assert!(inflight_spilled.is_none());
        assert_eq!(rq.depth.load(Ordering::Relaxed), 6);

        let queue = rq.queue.lock().unwrap_or_else(|e| e.into_inner());
        let queued_messages = queue
            .iter()
            .map(|req| req.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(queued_messages, vec!["first", "second", "third", "fourth"]);
        drop(queue);

        let spill = rq.spill.lock().unwrap_or_else(|e| e.into_inner());
        let restored = spill.inner.as_ref().expect("spill should be restored");
        assert_eq!(restored.pending_requests, 2);
        assert!(restored.paths.contains("spill-a.txt"));
        assert!(restored.paths.contains("spill-b.txt"));
    }

    #[test]
    fn wbq_zero_sync_fallbacks_under_normal_burst() {
        // Under a normal-sized burst (well below 8192 capacity), there should
        // be no queue-unavailable failures.
        wbq_start();

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

    #[test]
    fn wbq_can_restart_after_shutdown() {
        let wbq = new_write_behind_queue();

        wbq_start_inner(&wbq);
        let first_sender = wbq_sender_clone(&wbq).expect("sender should exist after start");
        assert_eq!(
            wbq_enqueue_with_sender(
                &first_sender,
                wbq.op_depth.as_ref(),
                wbq_test_clear_signal_op("wbq-restart-first"),
            ),
            WbqEnqueueResult::Enqueued
        );

        {
            let _lifecycle = wbq.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            first_sender
                .send(WbqMsg::Flush(done_tx))
                .expect("flush request should be delivered");
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("flush should complete");
            first_sender
                .send(WbqMsg::Shutdown)
                .expect("shutdown should be delivered");
            *wbq.sender.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let handle = {
                let mut guard = wbq.drain_handle.lock();
                guard.take()
            };
            if let Some(handle) = handle {
                handle.join().expect("drain thread should join cleanly");
            }
        }

        wbq_start_inner(&wbq);
        let second_sender = wbq_sender_clone(&wbq).expect("sender should exist after restart");
        assert_eq!(
            wbq_enqueue_with_sender(
                &second_sender,
                wbq.op_depth.as_ref(),
                wbq_test_clear_signal_op("wbq-restart-second"),
            ),
            WbqEnqueueResult::Enqueued
        );
        assert!(
            first_sender.send(WbqMsg::Shutdown).is_err(),
            "sender from the stopped worker must be disconnected after restart"
        );
    }

    #[test]
    fn test_process_markdown_images_rejects_invalid_local_image() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "md-invalid-proj").unwrap();

        let invalid_path = archive.root.join("broken.png");
        std::fs::write(&invalid_path, b"not a real image").unwrap();

        let err = process_markdown_images(
            &archive,
            &config,
            &archive.root,
            "Broken image: ![broken](broken.png)",
            EmbedPolicy::File,
        )
        .expect_err("invalid local markdown image should fail");

        assert!(matches!(err, StorageError::InvalidPath(_)));
        assert!(
            err.to_string().contains("decode image"),
            "expected decode failure, got {err}"
        );
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
    fn lockfree_commit_missing_file_records_deletion() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "lockfree-delete").unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();

        let file_path = archive.root.join("agents/TestAgent/profile.json");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, r#"{"name":"TestAgent"}"#).unwrap();
        let rel = rel_path_cached(&archive.canonical_repo_root, &file_path).unwrap();

        commit_paths_lockfree(&repo, &config, "lockfree add", &[rel.as_str()]).unwrap();
        fs::remove_file(&file_path).unwrap();
        commit_paths_lockfree(&repo, &config, "lockfree delete", &[rel.as_str()]).unwrap();

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert!(head.tree().unwrap().get_path(Path::new(&rel)).is_err());
    }

    #[test]
    fn commit_paths_missing_file_records_deletion() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "index-delete").unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();

        let file_path = archive.root.join("agents/TestAgent/profile.json");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, r#"{"name":"TestAgent"}"#).unwrap();
        let rel = rel_path_cached(&archive.canonical_repo_root, &file_path).unwrap();

        commit_paths(&repo, &config, "index add", &[rel.as_str()]).unwrap();
        fs::remove_file(&file_path).unwrap();
        commit_paths(&repo, &config, "index delete", &[rel.as_str()]).unwrap();

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert!(head.tree().unwrap().get_path(Path::new(&rel)).is_err());
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
        assert_eq!(
            lines.len(),
            3,
            "owner file should have PID, timestamp, and process start ticks"
        );

        let pid: u32 = lines[0].parse().unwrap();
        assert_eq!(
            pid,
            std::process::id(),
            "owner PID should match current process"
        );

        let ts: u64 = lines[1].parse().unwrap();
        assert!(ts > 0, "timestamp should be non-zero");
        let _start_ticks: u64 = lines[2].parse().unwrap();

        // Remove owner file
        remove_lock_owner(&archive.repo_root);
        assert!(!owner_path.exists(), "owner file should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn pid_owner_tracking_skips_symlinked_owner_leaf() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        let git_dir = repo_root.join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        let owner_path = git_dir.join("index.lock.owner");
        let outside = tmp.path().join("outside-owner.txt");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &owner_path).unwrap();

        write_lock_owner(&repo_root);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        remove_lock_owner(&repo_root);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(owner_path.exists(), "symlink leaf should be left untouched");
    }

    #[cfg(unix)]
    #[test]
    fn stale_lock_cleanup_skips_symlinked_git_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let outside_git = tmp.path().join("outside-git");
        fs::create_dir_all(&outside_git).unwrap();
        let outside_lock = outside_git.join("index.lock");
        let outside_owner = outside_git.join("index.lock.owner");
        fs::write(&outside_lock, b"fake lock").unwrap();
        fs::write(
            &outside_owner,
            b"999999999
1000000000
",
        )
        .unwrap();

        symlink(&outside_git, repo_root.join(".git")).unwrap();

        let removed = try_clean_stale_git_lock(&repo_root, 0.0);
        assert!(!removed, "symlinked .git directory must be ignored");
        assert_eq!(fs::read(&outside_lock).unwrap(), b"fake lock");
        assert_eq!(
            fs::read(&outside_owner).unwrap(),
            b"999999999
1000000000
"
        );
    }

    #[test]
    fn run_with_lock_owner_cleans_up_on_error() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "owner-cleanup-error").unwrap();
        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        let expected = StorageError::LockContention {
            message: "synthetic failure".to_string(),
        };

        let result: Result<()> = run_with_lock_owner(&archive.repo_root, || Err(expected));
        assert!(result.is_err(), "operation should fail");
        assert!(
            !owner_path.exists(),
            "owner sidecar must be removed even when operation fails"
        );
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

    #[test]
    fn stale_lock_dead_pid_owner_removed_before_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "dead-pid-order").unwrap();

        let lock_path = archive.repo_root.join(".git/index.lock");
        fs::write(&lock_path, "fake lock").unwrap();

        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        fs::write(&owner_path, "999999999\n1000000000\n").unwrap();

        let removed = try_clean_stale_git_lock(&archive.repo_root, 0.0);
        assert!(removed, "should remove lock owned by dead PID");
        assert!(!lock_path.exists(), "lock file should be removed");
        assert!(
            !owner_path.exists(),
            "owner metadata must not survive lock cleanup (TOCTOU invariant)"
        );
    }

    #[test]
    fn stale_lock_cleanup_alive_pid_unknown_start_ticks_not_removed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "alive-unknown-start").unwrap();

        let lock_path = archive.repo_root.join(".git/index.lock");
        fs::write(&lock_path, "fake lock").unwrap();

        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        let pid = std::process::id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simulate a new-format owner file where start ticks are unavailable.
        fs::write(&owner_path, format!("{pid}\n{now}\n0\n")).unwrap();

        // Use a large max_age so the age-based fallback (2x threshold) never
        // triggers for a freshly-created lock file.
        let removed = try_clean_stale_git_lock(&archive.repo_root, 3600.0);
        assert!(
            !removed,
            "must not remove lock owned by alive PID when start ticks are unknown"
        );
        assert!(lock_path.exists(), "lock file should still exist");

        fs::remove_file(&lock_path).ok();
        remove_lock_owner(&archive.repo_root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_lock_cleanup_removes_reused_pid_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let archive = ensure_archive(&config, "reused-pid").unwrap();

        let lock_path = archive.repo_root.join(".git/index.lock");
        fs::write(&lock_path, "fake lock").unwrap();

        let owner_path = archive.repo_root.join(".git/index.lock.owner");
        let pid = std::process::id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let fake_start_ticks = process_start_ticks(pid).unwrap_or(1).saturating_add(1);
        fs::write(&owner_path, format!("{pid}\n{now}\n{fake_start_ticks}\n")).unwrap();

        let removed = try_clean_stale_git_lock(&archive.repo_root, 300.0);
        assert!(removed, "should remove lock when PID start ticks mismatch");
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
            earliest_enqueued_wall: Utc::now(),
            latest_enqueued_wall: Utc::now(),
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
    #[ignore = "microbenchmark; unreliable under CI/parallel test load — run with --ignored"]
    fn spill_path_btreeset_benchmark_insert_and_drain() {
        use std::time::Instant;

        const SIZES: &[usize] = &[100, 500, 1_000, 4_096];
        const ITERATIONS: usize = 50;
        const MAX_OVERHEAD_FACTOR: f64 = 5.0; // BTreeSet must be < 5x of Vec+sort (relaxed for loaded CI)

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

    // -----------------------------------------------------------------------
    // now_iso() format tests
    // -----------------------------------------------------------------------

    #[test]
    fn now_iso_returns_rfc3339_format() {
        let ts = now_iso();
        // Must be parseable back as RFC 3339
        chrono::DateTime::parse_from_rfc3339(&ts).expect("now_iso() should return valid RFC 3339");
        // Should contain the year, 'T' separator, and timezone offset
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
        assert!(ts.contains('T'), "missing T separator: {ts}");
    }

    // -----------------------------------------------------------------------
    // list_message_files tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_message_files_nonexistent_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does_not_exist");
        let files = list_message_files(&nonexistent).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn list_message_files_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let files = list_message_files(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn list_message_files_finds_md_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        // Create .md files at multiple levels
        fs::write(dir.path().join("a.md"), "first").unwrap();
        let sub = dir.path().join("2026").join("02");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("b.md"), "second").unwrap();
        // Non-.md file should be excluded
        fs::write(dir.path().join("c.txt"), "skip").unwrap();

        let files = list_message_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2, "expected 2 .md files, got {}", files.len());
        // All returned paths should end in .md
        for f in &files {
            assert!(f.extension().is_some_and(|e| e == "md"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn list_message_files_skips_symlinked_root_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("escape.md"), "outside").unwrap();
        let linked = dir.path().join("linked");
        symlink(&outside, &linked).unwrap();

        let files = list_message_files(&linked).unwrap();
        assert!(
            files.is_empty(),
            "symlinked directory roots must not be traversed"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_attachment_source_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_attachment_source_path_empty_string_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let result = resolve_attachment_source_path(dir.path(), &config, "");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("empty"),
            "error should mention 'empty': {err_msg}"
        );
    }

    #[test]
    fn resolve_attachment_source_path_relative_inside_base() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        fs::write(dir.path().join("photo.png"), b"img").unwrap();
        let resolved = resolve_attachment_source_path(dir.path(), &config, "photo.png").unwrap();
        assert!(resolved.ends_with("photo.png"));
    }

    #[test]
    fn resolve_attachment_source_path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        // Create a file outside the base dir
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "data").unwrap();
        // Attempt traversal
        let result = resolve_attachment_source_path(dir.path(), &config, "../outside/secret.txt");
        // This should either not find the file or reject the traversal
        assert!(result.is_err(), "path traversal should be rejected");
    }

    #[test]
    fn resolve_attachment_source_path_nonexistent_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let result = resolve_attachment_source_path(dir.path(), &config, "no_such_file.txt");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("not found"),
            "error should mention 'not found': {err_msg}"
        );
    }

    // -----------------------------------------------------------------------
    // sanitize_browse_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_browse_path_rejects_traversal_variants() {
        assert!(sanitize_browse_path("..").is_err());
        assert!(sanitize_browse_path("../etc/passwd").is_err());
        assert!(sanitize_browse_path("foo/../../../etc").is_err());
        assert!(sanitize_browse_path("/absolute/path").is_err());
        assert!(sanitize_browse_path("foo/..").is_err());
    }

    #[test]
    fn sanitize_browse_path_allows_valid_paths() {
        assert_eq!(sanitize_browse_path("messages").unwrap(), "messages");
        assert_eq!(
            sanitize_browse_path("messages/2026/02").unwrap(),
            "messages/2026/02"
        );
        assert_eq!(sanitize_browse_path("").unwrap(), "");
    }

    #[test]
    fn sanitize_browse_path_normalizes_backslashes() {
        assert_eq!(
            sanitize_browse_path("messages\\2026\\02").unwrap(),
            "messages/2026/02"
        );
    }

    // -----------------------------------------------------------------------
    // check_archive_consistency edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn consistency_invalid_timestamp_counted_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let refs = vec![ConsistencyMessageRef {
            project_slug: "proj".into(),
            message_id: 1,
            sender_name: "Agent".into(),
            subject: "test".into(),
            created_ts_iso: "not-a-timestamp".into(),
        }];
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![1]);
    }

    #[test]
    fn consistency_missing_ids_capped_at_20() {
        let dir = tempfile::tempdir().unwrap();
        let refs: Vec<ConsistencyMessageRef> = (1..=30)
            .map(|i| ConsistencyMessageRef {
                project_slug: "proj".into(),
                message_id: i,
                sender_name: "Agent".into(),
                subject: format!("msg-{i}"),
                created_ts_iso: format!("2026-01-{:02}T00:00:00+00:00", (i % 28) + 1),
            })
            .collect();
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 30);
        assert_eq!(report.missing, 30);
        assert_eq!(
            report.missing_ids.len(),
            20,
            "missing_ids should be capped at 20"
        );
    }

    #[test]
    fn consistency_invalid_project_slug_counted_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let refs = vec![ConsistencyMessageRef {
            project_slug: "../escape".into(),
            message_id: 7,
            sender_name: "Agent".into(),
            subject: "test".into(),
            created_ts_iso: "2026-01-01T00:00:00+00:00".into(),
        }];

        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![7]);
    }

    #[cfg(unix)]
    #[test]
    fn consistency_symlinked_storage_root_counts_message_as_missing() {
        use std::os::unix::fs as unix_fs;

        let outside = tempfile::tempdir().unwrap();
        let msg_dir = outside
            .path()
            .join("projects")
            .join("proj")
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
        fs::write(
            msg_dir.join("2026-02-08T03-29-30+00-00__hello__42.md"),
            "content",
        )
        .unwrap();

        let wrapper = tempfile::tempdir().unwrap();
        let storage_root = wrapper.path().join("linked-root");
        unix_fs::symlink(outside.path(), &storage_root).unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: "proj".into(),
            message_id: 42,
            sender_name: "BlueLake".into(),
            subject: "hello".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];

        let report = check_archive_consistency(&storage_root, &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![42]);
    }

    #[test]
    fn consistency_matching_directory_is_not_treated_as_present() {
        let dir = tempfile::tempdir().unwrap();
        let msg_dir = dir
            .path()
            .join("projects")
            .join("proj")
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
        fs::create_dir(msg_dir.join("2026-02-08T03-29-30+00-00__hello__42.md")).unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: "proj".into(),
            message_id: 42,
            sender_name: "BlueLake".into(),
            subject: "hello".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];

        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![42]);
    }

    #[cfg(unix)]
    #[test]
    fn consistency_matching_symlink_file_is_not_treated_as_present() {
        use std::os::unix::fs as unix_fs;

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("message.md");
        fs::write(&outside_file, "content").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let msg_dir = dir
            .path()
            .join("projects")
            .join("proj")
            .join("messages")
            .join("2026")
            .join("02");
        fs::create_dir_all(&msg_dir).unwrap();
        unix_fs::symlink(
            &outside_file,
            msg_dir.join("2026-02-08T03-29-30+00-00__hello__42.md"),
        )
        .unwrap();

        let refs = vec![ConsistencyMessageRef {
            project_slug: "proj".into(),
            message_id: 42,
            sender_name: "BlueLake".into(),
            subject: "hello".into(),
            created_ts_iso: "2026-02-08T03:29:30+00:00".into(),
        }];

        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.missing_ids, vec![42]);
    }

    // ── Python archive compatibility verification (br-28mgh.5.1) ──────────

    /// Create a Python-format project.json and verify Rust can read it.
    #[test]
    fn compat_python_project_json_readable() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        fs::create_dir_all(&project_root).unwrap();

        // Python-format project.json (uses human_key, same as Rust)
        let project_json = serde_json::json!({
            "slug": "test-proj",
            "human_key": "/tmp/test-project"
        });
        fs::write(
            project_root.join("project.json"),
            serde_json::to_string_pretty(&project_json).unwrap(),
        )
        .unwrap();

        let content = fs::read_to_string(project_root.join("project.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["slug"], "test-proj");
        assert_eq!(parsed["human_key"], "/tmp/test-project");
    }

    /// Create a Python-format agent profile.json and verify read_agent_profile parses it.
    #[test]
    fn compat_python_agent_profile_readable() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let agent_dir = project_root.join("agents").join("RedFox");
        fs::create_dir_all(&agent_dir).unwrap();

        // Python-format profile.json
        let profile = serde_json::json!({
            "name": "RedFox",
            "program": "codex",
            "model": "gpt-5",
            "task_description": "Working on feature X",
            "inception_ts": "2026-02-15T06:47:55.258538Z",
            "last_active_ts": "2026-02-15T06:47:55.258538Z",
            "attachments_policy": "auto"
        });
        fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&profile).unwrap(),
        )
        .unwrap();

        let archive = ProjectArchive {
            slug: project_root.file_name().unwrap().to_string_lossy().into(),
            root: project_root.clone(),
            repo_root: dir.path().to_path_buf(),
            lock_path: project_root.join(".archive.lock"),
            canonical_repo_root: dir.path().to_path_buf(),
        };
        let result = read_agent_profile(&archive, "RedFox").unwrap();
        assert!(result.is_some(), "profile should be found");
        let val = result.unwrap();
        assert_eq!(val["name"], "RedFox");
        assert_eq!(val["program"], "codex");
        assert_eq!(val["model"], "gpt-5");
        assert_eq!(val["inception_ts"], "2026-02-15T06:47:55.258538Z");
    }

    /// Verify Python-format agent profile with missing optional fields parses gracefully.
    #[test]
    fn compat_python_agent_profile_missing_optional_fields() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let agent_dir = project_root.join("agents").join("BlueLake");
        fs::create_dir_all(&agent_dir).unwrap();

        // Minimal Python profile — no task_description, no attachments_policy
        let profile = serde_json::json!({
            "name": "BlueLake",
            "program": "claude-code",
            "model": "opus-4.6",
            "inception_ts": "2026-01-01T00:00:00Z",
            "last_active_ts": "2026-01-01T00:00:00Z"
        });
        fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&profile).unwrap(),
        )
        .unwrap();

        let archive = ProjectArchive {
            slug: project_root.file_name().unwrap().to_string_lossy().into(),
            root: project_root.clone(),
            repo_root: dir.path().to_path_buf(),
            lock_path: project_root.join(".archive.lock"),
            canonical_repo_root: dir.path().to_path_buf(),
        };
        let result = read_agent_profile(&archive, "BlueLake").unwrap();
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val["name"], "BlueLake");
        // Optional field should be null/missing, not an error
        assert!(
            val.get("task_description").is_none() || val["task_description"].is_null(),
            "missing optional field should not cause parse error"
        );
    }

    /// Verify Python-format message bundle (JSON frontmatter + Markdown body) is readable.
    #[test]
    fn compat_python_message_bundle_parseable() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let msg_dir = project_root.join("messages").join("2026").join("02");
        fs::create_dir_all(&msg_dir).unwrap();

        // Python-format message bundle
        let bundle = r#"---json
{
  "ack_required": false,
  "attachments": [],
  "bcc": [],
  "cc": [],
  "created": "2026-02-04T20:49:48.661479+00:00",
  "from": "FuchsiaForge",
  "id": 1081,
  "importance": "high",
  "project": "/data/projects/test-proj",
  "project_slug": "test-proj",
  "subject": "[PORT-PLAN] MCP Agent Mail Rust Port",
  "thread_id": "PORT-PLAN",
  "to": ["FuchsiaForge"]
}
---

# Message Body

This is a test message from the Python version.
"#;
        let filename = "2026-02-04T20-49-48Z__port-plan-mcp-agent-mail-rust-port__1081.md";
        fs::write(msg_dir.join(filename), bundle).unwrap();

        // Verify the file exists and is readable
        let content = fs::read_to_string(msg_dir.join(filename)).unwrap();
        assert!(
            content.contains("---json"),
            "should have JSON frontmatter marker"
        );

        // Extract and parse JSON frontmatter
        let json_start = content.find("---json\n").unwrap() + 8;
        let json_end = content[json_start..].find("\n---").unwrap() + json_start;
        let frontmatter = &content[json_start..json_end];
        let parsed: serde_json::Value = serde_json::from_str(frontmatter).unwrap();

        assert_eq!(parsed["from"], "FuchsiaForge");
        assert_eq!(parsed["id"], 1081);
        assert_eq!(parsed["thread_id"], "PORT-PLAN");
        assert_eq!(parsed["importance"], "high");
        assert!(
            parsed["to"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("FuchsiaForge"))
        );
    }

    /// Verify Python-format file reservation JSON is parseable by Rust.
    #[test]
    fn compat_python_file_reservation_parseable() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let res_dir = project_root.join("file_reservations");
        fs::create_dir_all(&res_dir).unwrap();

        // Python-format reservation (uses "path" key, not "path_pattern")
        let reservation_legacy = serde_json::json!({
            "id": 42,
            "agent": "RedFox",
            "path": "src/main.rs",
            "exclusive": true,
            "reason": "working on main module",
            "expires_ts": "2026-02-15T07:47:58.415682Z"
        });
        fs::write(
            res_dir.join("id-42.json"),
            serde_json::to_string_pretty(&reservation_legacy).unwrap(),
        )
        .unwrap();

        // Rust-format reservation (uses "path_pattern" key)
        let reservation_rust = serde_json::json!({
            "id": 43,
            "agent": "BlueLake",
            "path_pattern": "src/**/*.rs",
            "exclusive": false,
            "reason": "reading source files",
            "expires_ts": "2026-02-15T08:00:00Z"
        });
        fs::write(
            res_dir.join("id-43.json"),
            serde_json::to_string_pretty(&reservation_rust).unwrap(),
        )
        .unwrap();

        // Both should be parseable as JSON
        let legacy_content = fs::read_to_string(res_dir.join("id-42.json")).unwrap();
        let legacy: serde_json::Value = serde_json::from_str(&legacy_content).unwrap();
        assert_eq!(legacy["agent"], "RedFox");
        // Python uses "path" key
        assert_eq!(legacy["path"], "src/main.rs");

        let rust_content = fs::read_to_string(res_dir.join("id-43.json")).unwrap();
        let rust: serde_json::Value = serde_json::from_str(&rust_content).unwrap();
        assert_eq!(rust["agent"], "BlueLake");
        assert_eq!(rust["path_pattern"], "src/**/*.rs");
    }

    /// Verify Python timestamp formats are handled by parse_message_timestamp.
    #[test]
    fn compat_python_timestamp_formats() {
        // Python uses RFC3339 with timezone offset
        let msg1 = serde_json::json!({"created": "2026-02-04T20:49:48.661479+00:00"});
        let ts1 = parse_message_timestamp(&msg1);
        assert_eq!(ts1.year(), 2026);
        assert_eq!(ts1.month(), 2);
        assert_eq!(ts1.day(), 4);

        // Python also uses Z suffix
        let msg2 = serde_json::json!({"created": "2026-02-04T20:49:48Z"});
        let ts2 = parse_message_timestamp(&msg2);
        assert_eq!(ts2.year(), 2026);

        // Python naive datetime (no timezone info)
        let msg3 = serde_json::json!({"created": "2026-02-04T20:49:48.661479"});
        let ts3 = parse_message_timestamp(&msg3);
        assert_eq!(ts3.year(), 2026);

        // Python with space separator instead of T
        let msg4 = serde_json::json!({"created": "2026-02-04 20:49:48.661479"});
        let ts4 = parse_message_timestamp(&msg4);
        // Should parse or fall back to current time without panicking
        assert!(ts4.year() >= 2026);
    }

    /// Verify Python-written thread digest is readable as plain text.
    #[test]
    fn compat_python_thread_digest_readable() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let threads_dir = project_root.join("messages").join("threads");
        fs::create_dir_all(&threads_dir).unwrap();

        // Python-format thread digest (Markdown)
        let digest = r#"# Thread PORT-PLAN

## 2026-02-04T20:49:48+00:00 — FuchsiaForge → FuchsiaForge

[View canonical](../../../messages/2026/02/2026-02-04T20-49-48Z__port-plan__1081.md)

### [PORT-PLAN] MCP Agent Mail Rust Port

Hello fellow agents! I'm FuchsiaForge, starting the coordination thread.

---

## 2026-02-04T20:50:03+00:00 — IntroAgent → FuchsiaForge

[View canonical](../../../messages/2026/02/2026-02-04T20-50-03Z__re-introduction__1082.md)

### RE: Introduction

Thanks for setting this up! Let me know how I can help.

---
"#;
        fs::write(threads_dir.join("port-plan.md"), digest).unwrap();

        // Verify readable and contains expected structure
        let content = fs::read_to_string(threads_dir.join("port-plan.md")).unwrap();
        assert!(content.contains("# Thread PORT-PLAN"));
        assert!(content.contains("FuchsiaForge"));
        assert!(content.contains("IntroAgent"));
        assert!(content.contains("[View canonical]"));
    }

    /// Verify consistency check works against a Python-format archive.
    #[test]
    fn compat_python_archive_consistency_check() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("test-proj");
        let msg_dir = project_root.join("messages").join("2026").join("02");
        fs::create_dir_all(&msg_dir).unwrap();

        // Create a Python-format message at the expected canonical path
        let bundle = r#"---json
{"id": 100, "from": "TestAgent", "subject": "Test", "created": "2026-02-15T10:00:00+00:00", "to": ["Other"]}
---

Test body.
"#;
        fs::write(msg_dir.join("2026-02-15T10-00-00Z__test__100.md"), bundle).unwrap();

        // Check consistency — the message should be found
        let refs = vec![ConsistencyMessageRef {
            project_slug: "test-proj".into(),
            message_id: 100,
            sender_name: "TestAgent".into(),
            subject: "Test".into(),
            created_ts_iso: "2026-02-15T10:00:00+00:00".into(),
        }];
        let report = check_archive_consistency(dir.path(), &refs);
        assert_eq!(report.sampled, 1);
        assert_eq!(report.found, 1, "Python-format message should be found");
        assert_eq!(report.missing, 0);
    }

    /// Verify sha1-based file reservation artifacts (legacy Python naming) are valid JSON.
    #[test]
    fn compat_python_sha1_reservation_artifact_parseable() {
        let dir = TempDir::new().unwrap();
        let res_dir = dir
            .path()
            .join("projects")
            .join("test-proj")
            .join("file_reservations");
        fs::create_dir_all(&res_dir).unwrap();

        // Python uses SHA1 of path pattern as filename
        let path_pattern = "src/main.rs";
        let hash = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(path_pattern.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        let artifact = serde_json::json!({
            "id": 99,
            "agent": "PythonAgent",
            "path": path_pattern,
            "exclusive": true,
            "reason": "editing main module",
            "expires_ts": "2026-02-20T12:00:00Z"
        });
        fs::write(
            res_dir.join(format!("{hash}.json")),
            serde_json::to_string_pretty(&artifact).unwrap(),
        )
        .unwrap();

        // Verify the file is valid JSON and parseable
        let content = fs::read_to_string(res_dir.join(format!("{hash}.json"))).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["agent"], "PythonAgent");
        assert_eq!(parsed["path"], "src/main.rs");
    }

    /// Verify empty project (no messages, no agents) doesn't cause errors.
    #[test]
    fn compat_empty_project_no_errors() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("projects").join("empty-proj");
        fs::create_dir_all(&project_root).unwrap();

        let project_json = serde_json::json!({
            "slug": "empty-proj",
            "human_key": "/tmp/empty"
        });
        fs::write(
            project_root.join("project.json"),
            serde_json::to_string_pretty(&project_json).unwrap(),
        )
        .unwrap();

        let archive = ProjectArchive {
            slug: project_root.file_name().unwrap().to_string_lossy().into(),
            root: project_root.clone(),
            repo_root: dir.path().to_path_buf(),
            lock_path: project_root.join(".archive.lock"),
            canonical_repo_root: dir.path().to_path_buf(),
        };

        // read_agent_profile on nonexistent agent should return None, not error
        let result = read_agent_profile(&archive, "NoSuchAgent").unwrap();
        assert!(result.is_none());

        // Consistency check with empty refs should succeed
        let report = check_archive_consistency(dir.path(), &[]);
        assert_eq!(report.sampled, 0);
        assert_eq!(report.found, 0);
        assert_eq!(report.missing, 0);
    }

    // ────────────────────────────────────────────────────────────────
    // Archive gc defaults (A1-A5: loose-object accumulation mitigation)
    // ────────────────────────────────────────────────────────────────

    fn read_git_config_str(repo: &Repository, key: &str) -> Option<String> {
        let cfg = repo.config().ok()?;
        cfg.get_string(key).ok()
    }

    #[test]
    fn configure_archive_git_defaults_sets_gc_auto_on_fresh_repo() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).expect("init test repo");

        configure_archive_git_defaults(&repo);

        assert_eq!(
            read_git_config_str(&repo, "gc.auto").as_deref(),
            Some("256")
        );
        assert_eq!(
            read_git_config_str(&repo, "gc.autoPackLimit").as_deref(),
            Some("4")
        );
        assert_eq!(
            read_git_config_str(&repo, "gc.pruneExpire").as_deref(),
            Some("now")
        );
        assert_eq!(
            read_git_config_str(&repo, "maintenance.auto").as_deref(),
            Some("true")
        );
        assert_eq!(
            read_git_config_str(&repo, "repack.writeBitmaps").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn configure_archive_git_defaults_preserves_existing_operator_values() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).expect("init test repo");

        // Operator has tuned gc.auto to a non-default value. Our helper must
        // respect this and never overwrite it.
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_i64("gc.auto", 1000).unwrap();
            cfg.set_bool("maintenance.auto", false).unwrap();
        }

        configure_archive_git_defaults(&repo);

        assert_eq!(
            read_git_config_str(&repo, "gc.auto").as_deref(),
            Some("1000"),
            "operator-set gc.auto must not be overwritten"
        );
        assert_eq!(
            read_git_config_str(&repo, "maintenance.auto").as_deref(),
            Some("false"),
            "operator-set maintenance.auto must not be overwritten"
        );
        // Keys the operator did not touch still get our defaults.
        assert_eq!(
            read_git_config_str(&repo, "gc.autoPackLimit").as_deref(),
            Some("4")
        );
    }

    #[test]
    fn ensure_repo_applies_gc_defaults_on_fresh_init() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let config = test_config(&root);

        let created = ensure_repo(&root, &config).expect("ensure_repo");
        assert!(created, "expected fresh init to create a new repo");

        let repo = Repository::open(&root).expect("open newly-created repo");
        assert_eq!(
            read_git_config_str(&repo, "gc.auto").as_deref(),
            Some("256"),
            "fresh archive init must apply gc.auto=256"
        );
    }

    #[test]
    fn ensure_repo_migrates_gc_defaults_on_preexisting_archive() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        // Create a .git manually WITHOUT our gc defaults. A fresh
        // `Repository::init` never sets gc.auto locally, so when ensure_repo
        // later walks the "already exists" branch, `configure_archive_git_defaults`
        // will see the key as unset locally and apply our tuning.
        Repository::init(&root).expect("manual pre-existing init");

        // Force the per-process cache to miss so ensure_repo runs its
        // "already exists" branch (otherwise it returns Ok(false) early).
        repo_cache_clear_for_test();

        let config = test_config(&root);
        let created = ensure_repo(&root, &config).expect("ensure_repo");
        assert!(!created, "expected ensure_repo to see pre-existing .git");

        let repo = Repository::open(&root).expect("open migrated repo");
        assert_eq!(
            read_git_config_str(&repo, "gc.auto").as_deref(),
            Some("256"),
            "pre-existing archive must have gc.auto=256 migrated in"
        );
    }
}
