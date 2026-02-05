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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use git2::{Repository, Signature};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::Digest as Sha1Digest;
use thiserror::Error;

use mcp_agent_mail_core::config::Config;

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

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

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

                    // Exponential backoff with jitter
                    let base_ms = if attempt == 0 {
                        50
                    } else {
                        50 * (1u64 << attempt.min(4))
                    };
                    let jitter = (base_ms / 4) as i64;
                    let sleep_ms =
                        base_ms as i64 + (std::process::id() as i64 % (2 * jitter + 1)) - jitter;
                    let sleep_ms = sleep_ms.max(10) as u64;
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
pub fn with_project_lock<F, T>(archive: &ProjectArchive, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let mut lock = FileLock::new(archive.lock_path.clone());
    lock.acquire()?;
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

/// Statistics about commit queue operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitQueueStats {
    pub enqueued: usize,
    pub batched: usize,
    pub commits: usize,
    pub avg_batch_size: f64,
    pub queue_size: usize,
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
            let repo = Repository::open(&repo_root)?;
            let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
            commit_paths(&repo, config, &message, &refs)?;
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
                let repo = Repository::open(&repo_root)?;
                let refs: Vec<&str> = req.rel_paths.iter().map(String::as_str).collect();
                commit_paths(&repo, config, &req.message, &refs)?;
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

                    let repo = Repository::open(&repo_root)?;
                    let refs: Vec<&str> = merged_paths.iter().map(String::as_str).collect();
                    commit_paths(&repo, config, &combined, &refs)?;
                    self.record_commit(requests.len());
                } else {
                    // Conflicts or large batch - process sequentially
                    for req in &requests {
                        let repo = Repository::open(&repo_root)?;
                        let refs: Vec<&str> = req.rel_paths.iter().map(String::as_str).collect();
                        commit_paths(&repo, config, &req.message, &refs)?;
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
static COMMIT_QUEUE: Mutex<Option<CommitQueue>> = Mutex::new(None);

/// Get or create the global commit queue.
pub fn get_commit_queue() -> &'static Mutex<Option<CommitQueue>> {
    // Ensure initialized
    let mut guard = COMMIT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(CommitQueue::default());
    }
    drop(guard);
    &COMMIT_QUEUE
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
    msg.contains("index.lock") || msg.contains("lock at")
}

/// Try to clean up a stale .git/index.lock file.
///
/// Returns `true` if a stale lock was removed.
fn try_clean_stale_git_lock(repo_root: &Path, max_age_seconds: f64) -> bool {
    let lock_path = repo_root.join(".git").join("index.lock");
    if !lock_path.exists() {
        return false;
    }

    let age = fs::metadata(&lock_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs_f64());

    if let Some(age) = age {
        if age > max_age_seconds {
            let _ = fs::remove_file(&lock_path);
            return true;
        }
    }
    false
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

    let lock_path = commit_lock_path(repo_root, rel_paths);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Use project-scoped commit lock
    let mut lock = FileLock::new(lock_path);
    lock.acquire()?;

    let mut last_err_msg: Option<String> = None;
    let mut did_last_resort_clean = false;

    for attempt in 0..MAX_INDEX_LOCK_RETRIES + 2 {
        let repo = Repository::open(repo_root)?;
        match commit_paths(&repo, config, message, rel_paths) {
            Ok(()) => {
                lock.release()?;
                return Ok(());
            }
            Err(StorageError::Git(ref git_err)) if is_git_index_lock_error(git_err) => {
                last_err_msg = Some(git_err.message().to_string());

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
                lock.release()?;
                return Err(other);
            }
        }
    }

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
            let path = entry.path();
            if path.is_dir() {
                walk_for_locks(&path, result)?;
            } else if path.extension().is_some_and(|e| e == "lock") {
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
            let path = entry.path();
            if path.is_dir() {
                walk_for_orphaned_meta(&path, result)?;
            } else {
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
static REPO_CACHE: Mutex<Option<HashMap<PathBuf, bool>>> = Mutex::new(None);

fn repo_cache_contains(root: &Path) -> bool {
    let guard = REPO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().is_some_and(|m| m.contains_key(root))
}

fn repo_cache_insert(root: &Path) {
    let mut guard = REPO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(root.to_path_buf(), true);
}

// ---------------------------------------------------------------------------
// Archive initialization (br-2ei.2.1)
// ---------------------------------------------------------------------------

/// Ensure the global archive root directory exists and is a git repository.
///
/// Returns `(repo_root, was_freshly_initialized)`.
pub fn ensure_archive_root(config: &Config) -> Result<(PathBuf, bool)> {
    let root = config.storage_root.clone();
    fs::create_dir_all(&root)?;

    let fresh = ensure_repo(&root, config)?;
    Ok((root, fresh))
}

/// Ensure a per-project archive directory exists under the archive root.
pub fn ensure_archive(config: &Config, slug: &str) -> Result<ProjectArchive> {
    let (repo_root, _fresh) = ensure_archive_root(config)?;
    let project_root = repo_root.join("projects").join(slug);
    fs::create_dir_all(&project_root)?;

    Ok(ProjectArchive {
        slug: slug.to_string(),
        root: project_root.clone(),
        repo_root,
        lock_path: project_root.join(".archive.lock"),
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

    let profile_dir = archive.root.join("agents").join(name);
    fs::create_dir_all(&profile_dir)?;

    let profile_path = profile_dir.join("profile.json");
    write_json(&profile_path, agent)?;

    let rel = rel_path(&archive.repo_root, &profile_path)?;
    let repo = Repository::open(&archive.repo_root)?;
    commit_paths(
        &repo,
        // We need a minimal config for the commit author. Since we don't have
        // the full Config here, we use a placeholder. Callers should use the
        // version that accepts Config when they have it.
        &Config::default(),
        &format!("agent: profile {name}"),
        &[&rel],
    )?;

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

    let profile_dir = archive.root.join("agents").join(name);
    fs::create_dir_all(&profile_dir)?;

    let profile_path = profile_dir.join("profile.json");
    write_json(&profile_path, agent)?;

    let rel = rel_path(&archive.repo_root, &profile_path)?;
    let repo = Repository::open(&archive.repo_root)?;
    commit_paths(&repo, config, &format!("agent: profile {name}"), &[&rel])?;

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
    fs::create_dir_all(&reservation_dir)?;

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
        rel_paths.push(rel_path(&archive.repo_root, &legacy_path)?);

        // Stable per-reservation artifact: id-<id>.json
        if let Some(id) = normalized.get("id").and_then(serde_json::Value::as_i64) {
            let id_path = reservation_dir.join(format!("id-{id}.json"));
            write_json(&id_path, &normalized)?;
            rel_paths.push(rel_path(&archive.repo_root, &id_path)?);
        }

        let agent_name = normalized
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        entries.push((agent_name, path_pattern));
    }

    let commit_msg = build_file_reservation_commit_message(&entries);
    let repo = Repository::open(&archive.repo_root)?;
    let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
    commit_paths(&repo, config, &commit_msg, &refs)?;

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
        trimmed[..120].to_string()
    } else {
        trimmed
    };
    if truncated.is_empty() {
        "thread".to_string()
    } else {
        truncated
    }
}

/// Compute message archive paths for canonical, outbox, and inbox copies.
pub fn message_paths(
    archive: &ProjectArchive,
    sender: &str,
    recipients: &[String],
    created: &DateTime<Utc>,
    subject: &str,
    id: i64,
) -> MessageArchivePaths {
    let y = created.format("%Y").to_string();
    let m = created.format("%m").to_string();
    let iso = created.format("%Y-%m-%dT%H-%M-%SZ").to_string();

    let slug = {
        let raw = subject_slug_re().replace_all(subject, "-");
        let trimmed = raw
            .trim_matches(|c: char| c == '-' || c == '_')
            .to_lowercase();
        let truncated = if trimmed.len() > 80 {
            trimmed[..80].to_string()
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
    let inbox: Vec<PathBuf> = recipients
        .iter()
        .map(|r| {
            archive
                .root
                .join("agents")
                .join(r)
                .join("inbox")
                .join(&y)
                .join(&m)
                .join(&filename)
        })
        .collect();

    MessageArchivePaths {
        canonical,
        outbox,
        inbox,
    }
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
    );

    // Build frontmatter content
    let frontmatter = serde_json::to_string_pretty(message)?;
    let content = format!("---json\n{frontmatter}\n---\n\n{}\n", body_md.trim());

    // Create directories and write files
    let mut rel_paths = Vec::new();

    // Canonical
    if let Some(parent) = paths.canonical.parent() {
        fs::create_dir_all(parent)?;
    }
    write_text(&paths.canonical, &content)?;
    rel_paths.push(rel_path(&archive.repo_root, &paths.canonical)?);

    // Outbox
    if let Some(parent) = paths.outbox.parent() {
        fs::create_dir_all(parent)?;
    }
    write_text(&paths.outbox, &content)?;
    rel_paths.push(rel_path(&archive.repo_root, &paths.outbox)?);

    // Inbox copies
    for inbox_path in &paths.inbox {
        if let Some(parent) = inbox_path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_text(inbox_path, &content)?;
        rel_paths.push(rel_path(&archive.repo_root, inbox_path)?);
    }

    // Thread digest
    if let Some(thread_id) = message.get("thread_id").and_then(serde_json::Value::as_str) {
        let thread_id = thread_id.trim();
        if !thread_id.is_empty() {
            let canonical_rel = rel_path(&archive.repo_root, &paths.canonical)?;
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
        rel_paths.push(p.clone());
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

    let repo = Repository::open(&archive.repo_root)?;
    let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
    commit_paths(&repo, config, &commit_message, &refs)?;

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
    fs::create_dir_all(&digest_dir)?;

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
        format!("{}\n...", &preview[..1200].trim_end())
    } else {
        preview.to_string()
    };

    let entry = format!("{subject_line}{header}{link_line}{preview}\n\n---\n\n");

    // Append to digest
    let is_new = !digest_path.exists();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&digest_path)?;

    if is_new {
        file.write_all(format!("# Thread {thread_id}\n\n").as_bytes())?;
    }
    file.write_all(entry.as_bytes())?;

    rel_path(&archive.repo_root, &digest_path)
}

// ---------------------------------------------------------------------------
// Attachment pipeline
// ---------------------------------------------------------------------------

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

/// Store an image attachment in the archive.
///
/// Converts to WebP, writes to `attachments/{sha1[:2]}/{sha1}.webp`,
/// optionally keeps original, writes manifest and audit log.
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
    fs::create_dir_all(&webp_dir)?;
    fs::create_dir_all(&manifest_dir)?;
    fs::create_dir_all(&audit_dir)?;

    let mut rel_paths = Vec::new();

    // Convert to WebP
    let img = image::load_from_memory(&original_bytes)
        .map_err(|e| StorageError::InvalidPath(format!("Failed to decode image: {e}")))?;
    let (width, height) = img.dimensions();

    let webp_filename = format!("{digest}.webp");
    let webp_path = webp_dir.join(&webp_filename);

    // Encode to WebP using the image crate
    let mut webp_bytes = Vec::new();
    let rgba = img.to_rgba8();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
    encoder
        .encode(&rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| StorageError::InvalidPath(format!("WebP encode error: {e}")))?;

    fs::write(&webp_path, &webp_bytes)?;
    let webp_rel = rel_path(&archive.repo_root, &webp_path)?;
    rel_paths.push(webp_rel.clone());

    // Optionally keep original
    let original_rel = if config.keep_original_images {
        let orig_dir = attach_dir.join("originals").join(prefix);
        fs::create_dir_all(&orig_dir)?;
        let orig_path = orig_dir.join(format!("{digest}{original_ext}"));
        fs::write(&orig_path, &original_bytes)?;
        let rel = rel_path(&archive.repo_root, &orig_path)?;
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
    let manifest_path = manifest_dir.join(format!("{digest}.json"));
    write_json(&manifest_path, &serde_json::to_value(&manifest)?)?;
    rel_paths.push(rel_path(&archive.repo_root, &manifest_path)?);

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
    rel_paths.push(rel_path(&archive.repo_root, &audit_path)?);

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
/// Returns a list of attachment metadata and all relative paths written.
pub fn process_attachments(
    archive: &ProjectArchive,
    config: &Config,
    attachment_paths: &[String],
    embed_policy: EmbedPolicy,
) -> Result<(Vec<AttachmentMeta>, Vec<String>)> {
    let mut all_meta = Vec::new();
    let mut all_rel_paths = Vec::new();

    for path_str in attachment_paths {
        let path = PathBuf::from(path_str);
        let resolved = if path.is_absolute() {
            path
        } else {
            resolve_archive_relative_path(archive, path_str)?
        };

        if !resolved.exists() {
            return Err(StorageError::InvalidPath(format!(
                "Attachment not found: {}",
                resolved.display()
            )));
        }

        let stored = store_attachment(archive, config, &resolved, embed_policy)?;
        all_meta.push(stored.meta);
        all_rel_paths.extend(stored.rel_paths);
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
/// Finds `![alt](path)` references and replaces them with either:
/// - Inline base64 data URI: `![alt](data:image/webp;base64,...)`
/// - Archive file path: `![alt](attachments/ab/ab1234...webp)`
///
/// Returns the modified body and any attachment metadata/paths.
pub fn process_markdown_images(
    archive: &ProjectArchive,
    config: &Config,
    body_md: &str,
    embed_policy: EmbedPolicy,
) -> Result<(String, Vec<AttachmentMeta>, Vec<String>)> {
    let re = image_pattern_re();
    let mut all_meta = Vec::new();
    let mut all_rel_paths = Vec::new();
    let mut result = body_md.to_string();

    // Collect matches first to avoid borrow issues
    let matches: Vec<(String, String, String)> = re
        .captures_iter(body_md)
        .map(|cap| {
            let full = cap.get(0).unwrap().as_str().to_string();
            let alt = cap.name("alt").unwrap().as_str().to_string();
            let path = cap.name("path").unwrap().as_str().to_string();
            (full, alt, path)
        })
        .collect();

    for (full_match, alt, path) in matches {
        // Skip data URIs and URLs
        if path.starts_with("data:") || path.starts_with("http://") || path.starts_with("https://")
        {
            continue;
        }

        // Resolve the path
        let resolved = if Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            match resolve_archive_relative_path(archive, &path) {
                Ok(p) => p,
                Err(_) => continue, // Skip unresolvable paths
            }
        };

        if !resolved.exists() {
            continue;
        }

        match store_attachment(archive, config, &resolved, embed_policy) {
            Ok(stored) => {
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
            Err(_) => continue, // Skip failed conversions
        }
    }

    Ok((result, all_meta, all_rel_paths))
}

// ---------------------------------------------------------------------------
// Notification signals (legacy parity)
// ---------------------------------------------------------------------------

static SIGNAL_DEBOUNCE: OnceLock<Mutex<HashMap<(String, String), u128>>> = OnceLock::new();

fn signal_debounce() -> &'static Mutex<HashMap<(String, String), u128>> {
    SIGNAL_DEBOUNCE.get_or_init(|| Mutex::new(HashMap::new()))
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

    let debounce_ms = config.notifications_debounce_ms as u128;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let key = (project_slug.to_string(), agent_name.to_string());
    {
        let mut map = match signal_debounce().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
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
    let canonical_rel = rel_path(&archive.repo_root, &message_paths.canonical)?;
    find_commit_for_path(archive, &canonical_rel)
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
    let inbox_dir = archive.root.join("agents").join(agent_name).join("inbox");
    list_message_files(&inbox_dir)
}

/// Get outbox message files for a specific agent.
pub fn list_agent_outbox(archive: &ProjectArchive, agent_name: &str) -> Result<Vec<PathBuf>> {
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

    for path in rel_paths {
        // git2 expects forward-slash paths on all platforms
        let p = Path::new(path);
        // Only add if the file exists on disk
        let full = repo.workdir().ok_or(StorageError::NotInitialized)?.join(p);
        if full.exists() {
            index.add_path(p)?;
        }
    }

    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;

    // Append agent/thread trailers if applicable
    let final_message = append_trailers(message);

    // Find parent commit (if any)
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());

    match parent {
        Some(ref p) => {
            repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[p])?;
        }
        None => {
            repo.commit(Some("HEAD"), &sig, &sig, &final_message, &tree, &[])?;
        }
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

/// Compute a relative path from `base` to `target`.
fn rel_path(base: &Path, target: &Path) -> Result<String> {
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());

    target
        .strip_prefix(&base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| {
            StorageError::InvalidPath(format!(
                "Cannot compute relative path from {} to {}",
                base.display(),
                target.display()
            ))
        })
}

/// Resolve a relative path safely inside the project archive root.
///
/// Rejects directory traversal and ensures the path stays within the archive.
pub fn resolve_archive_relative_path(archive: &ProjectArchive, raw_path: &str) -> Result<PathBuf> {
    let normalized = raw_path.trim().replace('\\', "/");

    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.starts_with("..")
        || normalized.contains("/../")
        || normalized.ends_with("/..")
        || normalized == ".."
    {
        return Err(StorageError::InvalidPath(
            "directory traversal not allowed".to_string(),
        ));
    }

    let safe_rel = normalized.trim_start_matches('/');
    let root = archive
        .root
        .canonicalize()
        .unwrap_or_else(|_| archive.root.clone());
    let candidate = archive
        .root
        .join(safe_rel)
        .canonicalize()
        .unwrap_or_else(|_| archive.root.join(safe_rel));

    if !candidate.starts_with(&root) {
        return Err(StorageError::InvalidPath(
            "directory traversal not allowed".to_string(),
        ));
    }

    Ok(candidate)
}

/// Write text content to a file, creating parent directories as needed.
fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

/// Write JSON content to a file, creating parent directories as needed.
fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    fs::write(path, content)?;
    Ok(())
}

/// ISO 8601 timestamp for the current time.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(root: &Path) -> Config {
        Config {
            storage_root: root.to_path_buf(),
            ..Config::default()
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
        let rel = rel_path(&archive.repo_root, &test_file).unwrap();

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
            let rel = rel_path(&archive.repo_root, &file).unwrap();
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
            &[img1.display().to_string(), img2.display().to_string()],
            EmbedPolicy::File,
        )
        .unwrap();

        assert_eq!(meta.len(), 2);
        assert!(!rel_paths.is_empty());
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
            process_markdown_images(&archive, &config, body, EmbedPolicy::Inline).unwrap();

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
            process_markdown_images(&archive, &config, body, EmbedPolicy::File).unwrap();

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
}
