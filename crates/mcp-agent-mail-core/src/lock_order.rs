//! Lock ordering + debug-only deadlock prevention utilities.
//!
//! This module defines a **global lock hierarchy** for the small set of
//! process-global locks that may be acquired across subsystems (db/storage/tools).
//! At extreme concurrency, a single inconsistent acquisition order can deadlock
//! the entire process.
//!
//! Design goals:
//! - **Zero release overhead**: ordering checks compile to no-ops outside
//!   `debug_assertions`.
//! - **Fail fast in debug**: panic *before* attempting an out-of-order lock.
//! - **Incremental adoption**: wrap only the locks that matter.
//!
//! Rule (strict):
//! - When a thread already holds any lock(s), it may only acquire locks with a
//!   strictly higher `LockLevel::rank()`.
//!
//! If you need multiple locks, acquire them in ascending rank order, keep the
//! critical section tiny, and never hold these locks across blocking IO or `.await`.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Global lock hierarchy.
///
/// Lower rank must be acquired before higher rank when locks are nested.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LockLevel {
    // ---------------------------------------------------------------------
    // Database layer
    // ---------------------------------------------------------------------
    DbPoolCache,
    DbSqliteInitGates,
    DbReadCacheProjectsBySlug,
    DbReadCacheProjectsByHumanKey,
    DbReadCacheAgentsByKey,
    DbReadCacheAgentsById,
    DbReadCacheDeferredTouches,
    DbReadCacheLastTouchFlush,
    DbQueryTrackerInner,

    // ---------------------------------------------------------------------
    // Storage/archive layer
    // ---------------------------------------------------------------------
    StorageArchiveLockMap,
    StorageRepoCache,
    StorageSignalDebounce,
    StorageWbqDrainHandle,
    StorageWbqStats,
    StorageCommitQueue,

    // ---------------------------------------------------------------------
    // Tools layer
    // ---------------------------------------------------------------------
    ToolsBridgedEnv,
    ToolsToolMetrics,

    // ---------------------------------------------------------------------
    // Server layer (only a handful of process-global statics)
    // ---------------------------------------------------------------------
    ServerLiveDashboard,
}

impl LockLevel {
    /// Total order rank. Must be unique per variant.
    #[must_use]
    pub const fn rank(self) -> u16 {
        match self {
            // DB
            Self::DbPoolCache => 10,
            Self::DbSqliteInitGates => 11,
            Self::DbReadCacheProjectsBySlug => 20,
            Self::DbReadCacheProjectsByHumanKey => 21,
            Self::DbReadCacheAgentsByKey => 22,
            Self::DbReadCacheAgentsById => 23,
            Self::DbReadCacheDeferredTouches => 24,
            Self::DbReadCacheLastTouchFlush => 25,
            Self::DbQueryTrackerInner => 30,

            // Storage
            Self::StorageArchiveLockMap => 39,
            Self::StorageRepoCache => 40,
            Self::StorageSignalDebounce => 41,
            Self::StorageWbqDrainHandle => 50,
            Self::StorageWbqStats => 51,
            Self::StorageCommitQueue => 60,

            // Tools
            Self::ToolsBridgedEnv => 70,
            Self::ToolsToolMetrics => 80,

            // Server
            Self::ServerLiveDashboard => 90,
        }
    }
}

impl fmt::Display for LockLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}@{}", self.rank())
    }
}

#[cfg(debug_assertions)]
thread_local! {
    static HELD_LOCKS: RefCell<Vec<LockLevel>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn check_before_acquire(level: LockLevel) {
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| {
        let held = held.borrow();
        let Some(&last) = held.last() else {
            return;
        };
        assert!(
            level.rank() > last.rank(),
            "lock order violation: attempting to acquire {} while holding {}. held={:?}",
            level,
            last,
            held.as_slice()
        );
    });
}

#[inline]
fn did_acquire(level: LockLevel) {
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| held.borrow_mut().push(level));
}

#[inline]
fn did_release(level: LockLevel) {
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        let last = held.pop();
        assert!(
            last == Some(level),
            "lock tracking corrupted: expected to release {}, popped={:?}, held={:?}",
            level,
            last,
            held.as_slice()
        );
    });
}

/// Mutex wrapper that enforces the global lock hierarchy in debug builds.
#[derive(Debug)]
pub struct OrderedMutex<T> {
    level: LockLevel,
    inner: Mutex<T>,
}

impl<T> OrderedMutex<T> {
    #[must_use]
    pub const fn new(level: LockLevel, value: T) -> Self {
        Self {
            level,
            inner: Mutex::new(value),
        }
    }

    #[must_use]
    pub const fn level(&self) -> LockLevel {
        self.level
    }

    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        check_before_acquire(self.level);
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        did_acquire(self.level);
        OrderedMutexGuard {
            level: self.level,
            guard,
        }
    }

    #[allow(dead_code)]
    pub fn try_lock(&self) -> Option<OrderedMutexGuard<'_, T>> {
        check_before_acquire(self.level);
        let guard = self.inner.try_lock().ok()?;
        did_acquire(self.level);
        Some(OrderedMutexGuard {
            level: self.level,
            guard,
        })
    }
}

pub struct OrderedMutexGuard<'a, T> {
    level: LockLevel,
    guard: MutexGuard<'a, T>,
}

impl<T> Drop for OrderedMutexGuard<'_, T> {
    fn drop(&mut self) {
        did_release(self.level);
    }
}

impl<T> Deref for OrderedMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for OrderedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// `RwLock` wrapper that enforces the global lock hierarchy in debug builds.
#[derive(Debug)]
pub struct OrderedRwLock<T> {
    level: LockLevel,
    inner: RwLock<T>,
}

impl<T> OrderedRwLock<T> {
    #[must_use]
    pub const fn new(level: LockLevel, value: T) -> Self {
        Self {
            level,
            inner: RwLock::new(value),
        }
    }

    #[must_use]
    pub const fn level(&self) -> LockLevel {
        self.level
    }

    pub fn read(&self) -> OrderedRwLockReadGuard<'_, T> {
        check_before_acquire(self.level);
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        did_acquire(self.level);
        OrderedRwLockReadGuard {
            level: self.level,
            guard,
        }
    }

    pub fn write(&self) -> OrderedRwLockWriteGuard<'_, T> {
        check_before_acquire(self.level);
        let guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        did_acquire(self.level);
        OrderedRwLockWriteGuard {
            level: self.level,
            guard,
        }
    }
}

pub struct OrderedRwLockReadGuard<'a, T> {
    level: LockLevel,
    guard: RwLockReadGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        did_release(self.level);
    }
}

impl<T> Deref for OrderedRwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

pub struct OrderedRwLockWriteGuard<'a, T> {
    level: LockLevel,
    guard: RwLockWriteGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        did_release(self.level);
    }
}

impl<T> Deref for OrderedRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for OrderedRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn ordered_mutex_allows_increasing_order() {
        let pool_cache = OrderedMutex::new(LockLevel::DbPoolCache, ());
        let tool_metrics = OrderedMutex::new(LockLevel::ToolsToolMetrics, ());

        let _pool = pool_cache.lock();
        let _metrics = tool_metrics.lock();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn ordered_mutex_panics_on_out_of_order() {
        let tool_metrics = OrderedMutex::new(LockLevel::ToolsToolMetrics, ());
        let pool_cache = OrderedMutex::new(LockLevel::DbPoolCache, ());

        let _metrics = tool_metrics.lock();
        let _pool = pool_cache.lock();
    }

    #[test]
    fn stress_no_deadlock_under_contention_short() {
        let pool_cache = Arc::new(OrderedMutex::new(LockLevel::DbPoolCache, ()));
        let projects_by_slug =
            Arc::new(OrderedRwLock::new(LockLevel::DbReadCacheProjectsBySlug, ()));
        let query_tracker = Arc::new(OrderedMutex::new(LockLevel::DbQueryTrackerInner, ()));
        let wbq_stats = Arc::new(OrderedMutex::new(LockLevel::StorageWbqStats, ()));
        let tool_metrics = Arc::new(OrderedMutex::new(LockLevel::ToolsToolMetrics, ()));

        let start = Instant::now();
        let run_for = Duration::from_millis(150);
        let threads: usize = 100;

        let handles = (0..threads)
            .map(|_| {
                let pool_cache = Arc::clone(&pool_cache);
                let projects_by_slug = Arc::clone(&projects_by_slug);
                let query_tracker = Arc::clone(&query_tracker);
                let wbq_stats = Arc::clone(&wbq_stats);
                let tool_metrics = Arc::clone(&tool_metrics);
                thread::spawn(move || {
                    while start.elapsed() < run_for {
                        let _pool = pool_cache.lock();
                        let _projects = projects_by_slug.read();
                        let _queries = query_tracker.lock();
                        let _wbq = wbq_stats.lock();
                        let _metrics = tool_metrics.lock();
                    }
                })
            })
            .collect::<Vec<_>>();

        for h in handles {
            h.join().expect("thread panicked");
        }
    }
}
