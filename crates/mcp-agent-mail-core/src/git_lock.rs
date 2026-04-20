//! Per-repo git operation locking — Track B (br-8ujfs.2.*).
//!
//! Implements the two-layer hierarchy specified in
//! `docs/DESIGN_git_lock.md`:
//!
//! - [`GitRepoLocks`] — in-process `Mutex<HashMap<PathBuf,
//!   Arc<Mutex<()>>>>` keyed by canonical repo path. Serializes OUR
//!   threads.
//! - [`RepoFlock`] — OS-level `fcntl` flock on
//!   `<admin_dir>/am.git-serialize.lock`. Coordinates with peer
//!   processes that honor the same sentinel.
//!
//! Acquisition order: mutex first, flock second. Release reverses.
//!
//! # When to use
//!
//! Callers should prefer [`GitCmd`] (in [`super::git_cmd`], B4) which
//! combines both layers with `AM_GIT_BINARY` resolution and a
//! SIGSEGV-retry-capable runner. Direct use of [`GitRepoLocks`] /
//! [`RepoFlock`] is for special cases (e.g., Track F's
//! fix-orphan-refs holding the locks across a multi-step libgit2
//! operation).
//!
//! # Non-goals
//!
//! - Not reentrant. Same thread + same canonical path = panic.
//! - Not coordinating with the CommitCoalescer's per-repo CAS
//!   (that's a separate layer inside the archive's own write path;
//!   they don't overlap because the coalescer does libgit2 writes
//!   internally, not git shell-outs).
//! - Not protecting against unwrapped external git (e.g., user's
//!   IDE running `git commit`). That's mitigated by A5
//!   (`AM_GIT_BINARY`) + E1/E2 (SIGSEGV retry).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Default cap on how long `RepoFlock::acquire` may block before
/// aborting. Override with `AM_GIT_FLOCK_TIMEOUT_SECS` env var.
pub const DEFAULT_FLOCK_TIMEOUT_SECS: u64 = 60;

/// How often the acquire watchdog logs WARN while waiting for flock.
const FLOCK_WATCHDOG_TICK: Duration = Duration::from_secs(5);

/// Process-wide map of per-repo mutexes.
///
/// Keyed by the canonical repo path so that symlinked / relative paths
/// collapse to the same lock.
pub struct GitRepoLocks {
    inner: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl GitRepoLocks {
    /// Get the process-wide singleton.
    ///
    /// First call initializes a fresh empty map; subsequent calls reuse
    /// it. The outer `Mutex` guards map insertion only — actual lock
    /// acquisition happens on the inner `Arc<Mutex<()>>`.
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<GitRepoLocks> = OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Hand out the `Arc<Mutex<()>>` for the given (pre-canonicalized)
    /// repo path. Inserting if absent.
    #[must_use]
    pub fn lock_for(&self, canonical_repo: &Path) -> Arc<Mutex<()>> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entry(canonical_repo.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Test-only: wipe all entries. Keeps tests isolated.
    #[cfg(test)]
    pub fn reset_for_test(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }
}

/// Canonicalize a repo path robustly.
///
/// On success, returns `Some(canonical)`. On failure (nonexistent,
/// permission denied, etc.), returns `None` so callers can decide to
/// fall through without a lock.
#[must_use]
pub fn canonicalize_repo(repo: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(repo).ok()
}

/// Resolve the admin directory (`.git/`, bare repo root, or worktree's
/// admin dir) where the sentinel file belongs.
///
/// Returns `Some(dir)` if we can identify the admin dir; `None` if the
/// path is not a recognizable git repo shape (in which case the caller
/// should skip flock entirely).
#[must_use]
pub fn admin_dir_for(repo: &Path) -> Option<PathBuf> {
    // Prefer libgit2's understanding of admin dir (handles bare,
    // worktree, GIT_DIR env) but don't take a hard dep on git2 here —
    // we use a conservative heuristic that works for all common cases.
    //
    // Cases:
    //   1. <repo>/.git is a directory -> admin is <repo>/.git
    //   2. <repo>/.git is a file containing `gitdir: <path>` -> admin
    //      is that path.
    //   3. <repo> itself is the admin dir (bare repo): check for
    //      objects/ and refs/ subdirs.
    //   4. GIT_DIR env overrides everything.

    if let Ok(git_dir) = std::env::var("GIT_DIR")
        && !git_dir.trim().is_empty()
    {
        let p = PathBuf::from(git_dir);
        if p.is_dir() {
            return Some(p);
        }
    }

    let dot_git = repo.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        // Linked worktree: parse gitdir: <path>.
        if let Ok(text) = std::fs::read_to_string(&dot_git) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("gitdir:") {
                    let p = PathBuf::from(rest.trim());
                    if p.is_absolute() {
                        return Some(p);
                    }
                    // Relative gitdir resolves against the containing dir.
                    if let Some(parent) = dot_git.parent() {
                        return Some(parent.join(p));
                    }
                }
            }
        }
    }

    // Bare repo heuristic.
    if repo.join("objects").is_dir() && repo.join("refs").is_dir() {
        return Some(repo.to_path_buf());
    }

    None
}

/// Sentinel file path for flock coordination.
#[must_use]
pub fn sentinel_path(repo_canonical: &Path) -> Option<PathBuf> {
    admin_dir_for(repo_canonical).map(|ad| ad.join("am.git-serialize.lock"))
}

/// OS-level cooperative lock on a repo's sentinel file.
///
/// Acquired via `fcntl F_SETLK LOCK_EX` (fs2 crate). Released on drop.
///
/// If the sentinel can't be created (read-only `.git`, permissions,
/// network FS refusal), this returns a "phantom" RepoFlock that holds
/// no file and does nothing on drop. Callers are notified via
/// [`RepoFlock::is_real`].
#[derive(Debug)]
pub struct RepoFlock {
    /// Underlying file descriptor holding the lock. `None` = phantom.
    file: Option<File>,
    sentinel: Option<PathBuf>,
}

impl RepoFlock {
    /// Try to acquire the flock, blocking up to `timeout` total.
    ///
    /// # Errors
    ///
    /// - `io::ErrorKind::TimedOut`: waited the full timeout without
    ///   getting the lock. Caller should abort its git op rather than
    ///   retry forever.
    /// - `io::ErrorKind::PermissionDenied`: sentinel file cannot be
    ///   created; returns a phantom RepoFlock instead of an error
    ///   (see [`Self::try_acquire_phantom_on_failure`]).
    /// - Other IO errors propagate.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn acquire(repo_canonical: &Path) -> io::Result<Self> {
        let timeout = Duration::from_secs(flock_timeout_secs());
        Self::acquire_with_timeout(repo_canonical, timeout)
    }

    pub fn acquire_with_timeout(repo_canonical: &Path, timeout: Duration) -> io::Result<Self> {
        let Some(path) = sentinel_path(repo_canonical) else {
            tracing::debug!(
                target: "mcp_agent_mail::git_lock",
                repo = %repo_canonical.display(),
                "sentinel_path_unresolved_no_flock"
            );
            return Ok(Self {
                file: None,
                sentinel: None,
            });
        };

        // Ensure the parent (.git/) exists before we try to open.
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            tracing::debug!(
                target: "mcp_agent_mail::git_lock",
                parent = %parent.display(),
                "sentinel_parent_missing_no_flock"
            );
            return Ok(Self {
                file: None,
                sentinel: None,
            });
        }

        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied) => {
                tracing::warn!(
                    target: "mcp_agent_mail::git_lock",
                    sentinel = %path.display(),
                    err = %e,
                    "flock_readonly_fallback"
                );
                return Ok(Self {
                    file: None,
                    sentinel: Some(path),
                });
            }
            Err(e) => return Err(e),
        };

        // Try non-blocking first.
        match file.try_lock_exclusive() {
            Ok(()) => {
                tracing::debug!(
                    target: "mcp_agent_mail::git_lock",
                    sentinel = %path.display(),
                    "flock_acquired_nonblocking"
                );
                return Ok(Self {
                    file: Some(file),
                    sentinel: Some(path),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // fall through to blocking wait
            }
            Err(e) => return Err(e),
        }

        tracing::info!(
            target: "mcp_agent_mail::git_lock",
            sentinel = %path.display(),
            "flock_waiting"
        );

        // Blocking wait with watchdog ticks.
        let start = Instant::now();
        let mut last_tick = start;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    tracing::info!(
                        target: "mcp_agent_mail::git_lock",
                        sentinel = %path.display(),
                        wait_ms = start.elapsed().as_millis() as u64,
                        "flock_acquired_after_wait"
                    );
                    return Ok(Self {
                        file: Some(file),
                        sentinel: Some(path),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }

            let elapsed = start.elapsed();
            if elapsed >= timeout {
                tracing::error!(
                    target: "mcp_agent_mail::git_lock",
                    sentinel = %path.display(),
                    waited_s = elapsed.as_secs(),
                    "flock_timeout_aborting"
                );
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "flock on {} held by another process for {}s; aborting",
                        path.display(),
                        elapsed.as_secs()
                    ),
                ));
            }

            if last_tick.elapsed() >= FLOCK_WATCHDOG_TICK {
                tracing::warn!(
                    target: "mcp_agent_mail::git_lock",
                    sentinel = %path.display(),
                    waited_s = elapsed.as_secs(),
                    "flock_still_waiting"
                );
                last_tick = Instant::now();
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// True if this lock actually holds an fd; false for phantom locks
    /// that fell through due to permissions or missing admin dir.
    #[must_use]
    pub const fn is_real(&self) -> bool {
        self.file.is_some()
    }

    /// Path to the sentinel file, if any.
    #[must_use]
    pub fn sentinel_path(&self) -> Option<&Path> {
        self.sentinel.as_deref()
    }
}

impl Drop for RepoFlock {
    fn drop(&mut self) {
        if let Some(f) = &self.file
            && let Err(e) = f.unlock()
        {
            tracing::warn!(
                target: "mcp_agent_mail::git_lock",
                err = %e,
                sentinel = ?self.sentinel,
                "flock_unlock_failed"
            );
        }
    }
}

fn flock_timeout_secs() -> u64 {
    std::env::var("AM_GIT_FLOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_FLOCK_TIMEOUT_SECS)
}

// --- Reentrancy tracking ---------------------------------------------------

thread_local! {
    static HELD_LOCKS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// RAII guard that tracks held-lock state for reentrancy detection.
///
/// When `GitCmd::run` (B4) enters, it calls [`reentrancy_enter`] before
/// acquiring locks. If the repo path is already held by this thread,
/// we panic with a clear message — nested locking would deadlock.
pub struct ReentrancyGuard {
    path: PathBuf,
}

impl ReentrancyGuard {
    pub fn enter(repo_canonical: &Path) -> Self {
        HELD_LOCKS.with(|h| {
            let mut set = h.borrow_mut();
            if set.contains(repo_canonical) {
                panic!(
                    "run_git_locked reentrant call on {} from thread {:?}. \
                     This would deadlock. See docs/DESIGN_git_lock.md §6.1.",
                    repo_canonical.display(),
                    std::thread::current().id()
                );
            }
            set.insert(repo_canonical.to_path_buf());
        });
        Self {
            path: repo_canonical.to_path_buf(),
        }
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        HELD_LOCKS.with(|h| {
            let mut set = h.borrow_mut();
            set.remove(&self.path);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) -> PathBuf {
        let p = dir.join("repo");
        std::fs::create_dir_all(p.join(".git/objects")).unwrap();
        std::fs::create_dir_all(p.join(".git/refs")).unwrap();
        std::fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        p
    }

    #[test]
    fn same_repo_two_threads_serialize() {
        GitRepoLocks::global().reset_for_test();
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();

        // Each thread increments N times inside the lock; if the lock
        // were broken we'd see lost-update races. We assert strict
        // equality which only holds with serialization.
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 250;
        let counter = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let mut expected_next = 0u64;
                let _ = &mut expected_next;
                let c = Arc::clone(&counter);
                let lk = GitRepoLocks::global().lock_for(&canonical);
                thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        let _g = lk.lock().unwrap();
                        let prev = c.load(Ordering::Relaxed);
                        // Non-atomic RMW under the lock.
                        c.store(prev + 1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), THREADS * PER_THREAD);
    }

    #[test]
    fn different_repos_parallel() {
        GitRepoLocks::global().reset_for_test();
        let tmp = TempDir::new().unwrap();
        let repo_a = init_repo(&tmp.path().join("a"));
        let repo_b = init_repo(&tmp.path().join("b"));
        let ca = canonicalize_repo(&repo_a).unwrap();
        let cb = canonicalize_repo(&repo_b).unwrap();

        // Two different repos should map to two different mutexes.
        let la = GitRepoLocks::global().lock_for(&ca);
        let lb = GitRepoLocks::global().lock_for(&cb);
        assert!(!Arc::ptr_eq(&la, &lb));
    }

    #[test]
    fn canonical_paths_collapse_to_same_mutex() {
        GitRepoLocks::global().reset_for_test();
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();

        // Same canonical path on two lookups -> same Arc.
        let l1 = GitRepoLocks::global().lock_for(&canonical);
        let l2 = GitRepoLocks::global().lock_for(&canonical);
        assert!(Arc::ptr_eq(&l1, &l2));
    }

    #[test]
    fn flock_basic_acquire_release() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();

        let f1 = RepoFlock::acquire(&canonical).unwrap();
        assert!(f1.is_real(), "real flock expected on a proper repo");

        // Second acquire from same process — try_lock would fail
        // immediately; we use a tight timeout to avoid blocking the test.
        let t0 = Instant::now();
        let res = RepoFlock::acquire_with_timeout(&canonical, Duration::from_millis(200));
        let elapsed = t0.elapsed();
        match res {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                assert!(
                    elapsed >= Duration::from_millis(150),
                    "should have waited close to timeout, got {elapsed:?}"
                );
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }

        drop(f1);
        // After drop, acquire succeeds.
        let _f2 = RepoFlock::acquire_with_timeout(&canonical, Duration::from_millis(500))
            .expect("acquire after drop");
    }

    #[test]
    fn flock_on_nonrepo_path_returns_phantom() {
        let tmp = TempDir::new().unwrap();
        // Not a repo: no .git/ dir.
        let not_a_repo = tmp.path().join("plain-dir");
        std::fs::create_dir(&not_a_repo).unwrap();
        let canonical = canonicalize_repo(&not_a_repo).unwrap();
        let flock = RepoFlock::acquire(&canonical).expect("phantom flock ok");
        assert!(!flock.is_real(), "should be phantom (no admin dir)");
    }

    #[test]
    fn admin_dir_resolves_bare_repo() {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("bare.git");
        std::fs::create_dir_all(bare.join("objects")).unwrap();
        std::fs::create_dir_all(bare.join("refs")).unwrap();
        assert_eq!(admin_dir_for(&bare), Some(bare.clone()));
    }

    #[test]
    fn admin_dir_resolves_linked_worktree() {
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/feat/objects")).unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/feat").display()),
        )
        .unwrap();
        let admin = admin_dir_for(&work).expect("admin dir resolvable");
        assert!(
            admin.ends_with("main/.git/worktrees/feat"),
            "got: {}",
            admin.display()
        );
    }

    #[test]
    #[should_panic(expected = "reentrant call")]
    fn reentrancy_same_thread_panics() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let _g1 = ReentrancyGuard::enter(&canonical);
        let _g2 = ReentrancyGuard::enter(&canonical); // should panic
    }

    #[test]
    fn reentrancy_allows_different_repos() {
        let tmp = TempDir::new().unwrap();
        let a = init_repo(&tmp.path().join("a"));
        let b = init_repo(&tmp.path().join("b"));
        let ca = canonicalize_repo(&a).unwrap();
        let cb = canonicalize_repo(&b).unwrap();
        let _g1 = ReentrancyGuard::enter(&ca);
        let _g2 = ReentrancyGuard::enter(&cb); // must NOT panic
    }

    #[test]
    fn reentrancy_allows_sequential_reacquire() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        {
            let _g1 = ReentrancyGuard::enter(&canonical);
        } // dropped
        let _g2 = ReentrancyGuard::enter(&canonical); // no panic
    }

    #[test]
    fn reentrancy_independent_across_threads() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let _g1 = ReentrancyGuard::enter(&canonical);
        // Second thread acquiring the SAME path must not panic —
        // reentrancy is per-thread.
        let cc = canonical.clone();
        thread::spawn(move || {
            let _g2 = ReentrancyGuard::enter(&cc);
        })
        .join()
        .unwrap();
    }
}
