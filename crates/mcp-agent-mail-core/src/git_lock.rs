//! Per-repo git operation locking — Track B (br-8ujfs.2.*).
//!
//! Implements the two-layer hierarchy specified in
//! `docs/DESIGN_git_lock.md`:
//!
//! - [`GitRepoLocks`] — in-process `Mutex<HashMap<PathBuf,
//!   Arc<Mutex<()>>>>` keyed by canonical repo path. Serializes OUR
//!   threads.
//! - [`RepoFlock`] — OS-level cooperative lock on
//!   `<admin_dir>/am.git-serialize.lock`. Coordinates with peer
//!   processes that honor the same sentinel.
//!
//! Acquisition order: mutex first, flock second. Release reverses.
//! Timed admission uses one monotonic budget, including interruptions and
//! diagnostic time. It cannot preempt an individual filesystem syscall.
//!
//! # When to use
//!
//! Callers should prefer [`GitCmd`](super::git_cmd::GitCmd) (B4), which
//! combines both layers with `AM_GIT_BINARY` resolution and a
//! SIGSEGV-retry-capable runner. Direct use of [`GitRepoLocks`] /
//! [`RepoFlock`] is for special cases (e.g., Track F's
//! fix-orphan-refs holding the locks across a multi-step libgit2
//! operation).
//!
//! # Non-goals
//!
//! - Not reentrant. Same thread + same canonical path = panic.
//! - Not coordinating with the `CommitCoalescer`'s per-repo CAS
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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Default cap on how long `RepoFlock::acquire` may block before
/// aborting. Override with `AM_GIT_FLOCK_TIMEOUT_SECS` env var.
pub const DEFAULT_FLOCK_TIMEOUT_SECS: u64 = 60;

/// How often the acquire watchdog logs WARN while waiting for flock.
const FLOCK_WATCHDOG_TICK: Duration = Duration::from_secs(5);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(5);

fn lock_timeout(timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("git lock admission exceeded {timeout:?}; operation was not admitted"),
    )
}

/// Shared polling policy for mutexes and advisory locks. The elapsed callback
/// starts BEFORE setup, not after the first failed acquisition. A returned
/// value owns any acquired resource, so rejecting a late success drops it.
/// The flock caller retains its file until this function returns, including
/// error returns, and releases its advisory lock by dropping that file.
fn wait_for_lock<T>(
    timeout: Duration,
    mut elapsed: impl FnMut() -> Duration,
    mut acquire: impl FnMut() -> io::Result<Option<T>>,
    mut pause: impl FnMut(Duration),
) -> io::Result<T> {
    let mut first = true;
    loop {
        // Zero means one immediate try, not an unbounded wait. Positive
        // budgets do not get a fresh try after setup has exhausted them.
        if (!first || !timeout.is_zero()) && elapsed() >= timeout {
            return Err(lock_timeout(timeout));
        }
        first = false;
        match acquire() {
            Ok(Some(value)) => {
                if !timeout.is_zero() && elapsed() >= timeout {
                    drop(value);
                    return Err(lock_timeout(timeout));
                }
                return Ok(value);
            }
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        let remaining = timeout.saturating_sub(elapsed());
        if remaining.is_zero() {
            return Err(lock_timeout(timeout));
        }
        pause(remaining.min(LOCK_RETRY_INTERVAL));
    }
}

/// Bound an in-process lock wait without delegating it to an orphanable thread.
/// Poison recovery preserves the existing state; no callback logs under the lock.
pub(crate) fn lock_mutex_with_timeout<T>(
    mutex: &Mutex<T>,
    timeout: Duration,
) -> io::Result<MutexGuard<'_, T>> {
    let started = Instant::now();
    wait_for_lock(
        timeout,
        || started.elapsed(),
        || match mutex.try_lock() {
            Ok(guard) => Ok(Some(guard)),
            Err(TryLockError::Poisoned(error)) => Ok(Some(error.into_inner())),
            Err(TryLockError::WouldBlock) => Ok(None),
        },
        std::thread::sleep,
    )
}

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

    /// Like [`Self::lock_for`], but the registry wait consumes the caller's
    /// remaining budget. This does not acquire the returned repository mutex.
    /// A timeout never evicts or replaces another caller's lock identity.
    pub fn lock_for_with_timeout(
        &self,
        canonical_repo: &Path,
        timeout: Duration,
    ) -> io::Result<Arc<Mutex<()>>> {
        let mut guard = lock_mutex_with_timeout(&self.inner, timeout)?;
        Ok(Arc::clone(
            guard
                .entry(canonical_repo.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    /// Test-only: wipe all entries. Must not race callers using this registry.
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
    std::fs::canonicalize(repo)
        .ok()
        .map(|canonical| crate::disk::simplify_verbatim_path(&canonical))
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

/// OS-level cooperative lock on a repo's sentinel file, released on drop.
///
/// If the sentinel can't be created due to permissions, this returns a
/// "phantom" `RepoFlock` holding no file. Callers can inspect [`Self::is_real`].
/// Other failures, including contention timeout, remain errors.
#[derive(Debug)]
pub struct RepoFlock {
    /// Underlying file descriptor holding the lock. `None` = phantom.
    file: Option<File>,
    sentinel: Option<PathBuf>,
}

fn is_lock_contention(error: &io::Error) -> bool {
    // Windows reports ERROR_LOCK_VIOLATION, which is not WouldBlock.
    // Compare the platform code rather than its broad ErrorKind so unrelated
    // Windows errors still propagate instead of being retried until timeout.
    error.kind() == io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
}

impl RepoFlock {
    /// Acquire with the configured contention budget. Permission-denied or
    /// unrecognized repositories retain the explicit phantom-lock behavior.
    /// Contention timeout and other I/O errors are returned to the caller.
    pub fn acquire(repo_canonical: &Path) -> io::Result<Self> {
        let timeout = Duration::from_secs(flock_timeout_secs());
        Self::acquire_with_timeout(repo_canonical, timeout)
    }

    /// Acquire under one monotonic budget, starting before filesystem setup.
    /// A zero budget permits one nonblocking attempt and never sleeps. A
    /// positive expired budget cannot be renewed by a late lock release.
    #[allow(clippy::too_many_lines)]
    pub fn acquire_with_timeout(repo_canonical: &Path, timeout: Duration) -> io::Result<Self> {
        let started = Instant::now();
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
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                tracing::warn!(
                    target: "mcp_agent_mail::git_lock",
                    sentinel = %path.display(),
                    err = %error,
                    "flock_readonly_fallback"
                );
                return Ok(Self {
                    file: None,
                    sentinel: Some(path),
                });
            }
            Err(error) => return Err(error),
        };

        let mut last_tick = started;
        let result = wait_for_lock(
            timeout,
            || started.elapsed(),
            || match file.try_lock_exclusive() {
                Ok(()) => Ok(Some(())),
                Err(error) if is_lock_contention(&error) => Ok(None),
                Err(error) => Err(error),
            },
            |pause| {
                if last_tick.elapsed() >= FLOCK_WATCHDOG_TICK {
                    tracing::warn!(
                        target: "mcp_agent_mail::git_lock",
                        sentinel = %path.display(),
                        waited_ms = duration_ms_u64(started.elapsed()),
                        "flock_still_waiting"
                    );
                    last_tick = Instant::now();
                }
                // A slow diagnostic subscriber also spends the same budget.
                std::thread::sleep(pause.min(timeout.saturating_sub(started.elapsed())));
            },
        );
        if let Err(error) = result {
            // Dropping the file also releases a lock acquired just too late.
            drop(file);
            return Err(error);
        }
        tracing::debug!(
            target: "mcp_agent_mail::git_lock",
            sentinel = %path.display(),
            wait_ms = duration_ms_u64(started.elapsed()),
            "flock_acquired"
        );
        Ok(Self {
            file: Some(file),
            sentinel: Some(path),
        })
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

pub(crate) fn flock_timeout_secs() -> u64 {
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
/// When `GitCmd::run` enters, it calls [`Self::enter`] before acquiring locks.
/// A nested acquisition of the same canonical repository would deadlock.
pub struct ReentrancyGuard {
    path: PathBuf,
}

impl ReentrancyGuard {
    #[must_use]
    pub fn enter(repo_canonical: &Path) -> Self {
        HELD_LOCKS.with(|h| {
            let mut set = h.borrow_mut();
            assert!(
                !set.contains(repo_canonical),
                "run_git_locked reentrant call on {} from thread {:?}. \
                 This would deadlock. See docs/DESIGN_git_lock.md §6.1.",
                repo_canonical.display(),
                std::thread::current().id()
            );
            set.insert(repo_canonical.to_path_buf());
        });
        Self {
            path: repo_canonical.to_path_buf(),
        }
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    use std::cell::Cell;
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

    fn isolated_locks() -> GitRepoLocks {
        GitRepoLocks {
            inner: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn same_repo_two_threads_serialize() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 250;

        let locks = isolated_locks();
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let counter = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let c = Arc::clone(&counter);
                let lk = locks.lock_for(&canonical);
                thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        let _g = lk.lock().unwrap();
                        let prev = c.load(Ordering::Relaxed);
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
        let locks = isolated_locks();
        let tmp = TempDir::new().unwrap();
        let repo_a = init_repo(&tmp.path().join("a"));
        let repo_b = init_repo(&tmp.path().join("b"));
        let ca = canonicalize_repo(&repo_a).unwrap();
        let cb = canonicalize_repo(&repo_b).unwrap();
        let la = locks.lock_for(&ca);
        let lb = locks.lock_for(&cb);
        assert!(!Arc::ptr_eq(&la, &lb));
    }

    #[test]
    fn canonical_paths_collapse_to_same_mutex() {
        let locks = isolated_locks();
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let l1 = locks.lock_for(&canonical);
        let l2 = locks.lock_for(&canonical);
        assert!(Arc::ptr_eq(&l1, &l2));
    }

    #[test]
    fn flock_basic_acquire_release() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let f1 = RepoFlock::acquire(&canonical).unwrap();
        assert!(f1.is_real(), "real flock expected on a proper repo");
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(f1.sentinel_path().unwrap())
            .unwrap();
        let contention = contender.try_lock_exclusive().unwrap_err();
        assert!(is_lock_contention(&contention), "{contention:?}");
        assert!(!is_lock_contention(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!is_lock_contention(&io::Error::other(
            "unrelated lock failure"
        )));
        drop(contender);
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
        let _f2 = RepoFlock::acquire_with_timeout(&canonical, Duration::from_millis(500))
            .expect("acquire after drop");
    }

    #[test]
    fn flock_on_nonrepo_path_returns_phantom() {
        let tmp = TempDir::new().unwrap();
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
        let _g2 = ReentrancyGuard::enter(&canonical);
    }

    #[test]
    fn reentrancy_allows_different_repos() {
        let tmp = TempDir::new().unwrap();
        let a = init_repo(&tmp.path().join("a"));
        let b = init_repo(&tmp.path().join("b"));
        let ca = canonicalize_repo(&a).unwrap();
        let cb = canonicalize_repo(&b).unwrap();
        let _g1 = ReentrancyGuard::enter(&ca);
        let _g2 = ReentrancyGuard::enter(&cb);
    }

    #[test]
    fn reentrancy_allows_sequential_reacquire() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        {
            let _g1 = ReentrancyGuard::enter(&canonical);
        }
        let _g2 = ReentrancyGuard::enter(&canonical);
    }

    #[test]
    fn reentrancy_independent_across_threads() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let _g1 = ReentrancyGuard::enter(&canonical);
        let cc = canonical;
        thread::spawn(move || {
            let _g2 = ReentrancyGuard::enter(&cc);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn expired_positive_budget_never_attempts_acquisition() {
        let calls = Cell::new(0);
        let result = wait_for_lock(
            Duration::from_millis(2),
            || Duration::from_millis(2),
            || {
                calls.set(calls.get() + 1);
                Ok(Some(()))
            },
            |_| panic!("an expired budget must not sleep"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn zero_budget_makes_exactly_one_nonblocking_attempt() {
        for ready in [false, true] {
            let calls = Cell::new(0);
            let result = wait_for_lock(
                Duration::ZERO,
                || Duration::from_secs(1),
                || {
                    calls.set(calls.get() + 1);
                    Ok(ready.then_some(7))
                },
                |_| panic!("zero budget must not sleep"),
            );
            assert_eq!(calls.get(), 1);
            assert_eq!(result.is_ok(), ready);
        }
    }

    #[test]
    fn overslept_budget_cannot_accept_a_late_release() {
        let elapsed = Cell::new(Duration::ZERO);
        let calls = Cell::new(0);
        let result = wait_for_lock(
            Duration::from_millis(2),
            || elapsed.get(),
            || {
                calls.set(calls.get() + 1);
                Ok((calls.get() > 1).then_some(()))
            },
            |pause| {
                assert_eq!(pause, Duration::from_millis(2));
                elapsed.set(Duration::from_millis(3));
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn late_success_drops_its_acquired_resource() {
        let elapsed = Cell::new(Duration::ZERO);
        let resource = Arc::new(());
        let result = wait_for_lock(
            Duration::from_millis(2),
            || elapsed.get(),
            || {
                elapsed.set(Duration::from_millis(2));
                Ok(Some(Arc::clone(&resource)))
            },
            |_| panic!("successful acquisition must not sleep"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(Arc::strong_count(&resource), 1);
    }

    #[test]
    fn interrupted_acquisitions_share_the_same_budget() {
        let elapsed = Cell::new(Duration::ZERO);
        let calls = Cell::new(0);
        let result: io::Result<()> = wait_for_lock(
            Duration::from_millis(7),
            || elapsed.get(),
            || {
                calls.set(calls.get() + 1);
                Err(io::ErrorKind::Interrupted.into())
            },
            |pause| elapsed.set(elapsed.get() + pause),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(elapsed.get(), Duration::from_millis(7));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unrelated_lock_error_is_not_retried() {
        let result: io::Result<()> = wait_for_lock(
            Duration::MAX,
            || Duration::ZERO,
            || Err(io::ErrorKind::PermissionDenied.into()),
            |_| panic!("unrelated error must not retry"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn timed_mutex_preserves_holder_and_recovers_after_release() {
        let mutex = Mutex::new(42);
        let held = mutex.lock().unwrap();
        let started = Instant::now();
        let error = lock_mutex_with_timeout(&mutex, Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(*held, 42);
        drop(held);
        assert_eq!(*lock_mutex_with_timeout(&mutex, Duration::ZERO).unwrap(), 42);
    }

    #[test]
    fn timed_mutex_recovers_poison_without_losing_state() {
        let mutex = Mutex::new(42);
        let result = std::panic::catch_unwind(|| {
            let _held = mutex.lock().unwrap();
            panic!("fixture poison");
        });
        assert!(result.is_err());
        assert_eq!(*lock_mutex_with_timeout(&mutex, Duration::MAX).unwrap(), 42);
    }

    #[test]
    fn registry_timeout_cannot_replace_an_existing_mutex() {
        let locks = isolated_locks();
        let path = Path::new("repo");
        let original = locks.lock_for(path);
        let held = locks.inner.lock().unwrap();
        let error = locks.lock_for_with_timeout(path, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(held);
        let subsequent = locks.lock_for_with_timeout(path, Duration::MAX).unwrap();
        assert!(Arc::ptr_eq(&original, &subsequent));
    }

    #[test]
    fn flock_zero_budget_and_maximum_budget_preserve_real_exclusion() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let held = RepoFlock::acquire_with_timeout(&repo, Duration::ZERO).unwrap();
        assert!(held.is_real());
        assert_eq!(
            RepoFlock::acquire_with_timeout(&repo, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        drop(held);
        assert!(RepoFlock::acquire_with_timeout(&repo, Duration::MAX).unwrap().is_real());
    }
}
