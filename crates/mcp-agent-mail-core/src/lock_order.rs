//! Lock ordering + debug-only deadlock prevention + contention instrumentation.
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
//! - **Contention visibility**: always-on lightweight tracking of acquire counts,
//!   contention events, wait times, and hold durations. Uses `try_lock()` first
//!   so uncontended acquires add only ~2 atomic increments (~2-4ns overhead).
//!
//! Rule (strict):
//! - When a thread already holds any lock(s), it may only acquire locks with a
//!   strictly higher `LockLevel::rank()`.
//!
//! If you need multiple locks, acquire them in ascending rank order, keep the
//! critical section tiny, and never hold these locks across blocking IO or `.await`.

#![forbid(unsafe_code)]

#[cfg(debug_assertions)]
use std::cell::RefCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Extension trait for `Duration` that converts to nanoseconds as `u64`,
/// saturating to `u64::MAX` for extremely long durations (>585 years).
trait DurationNanosU64 {
    fn as_nanos_u64(&self) -> u64;
}

impl DurationNanosU64 for std::time::Duration {
    #[inline]
    fn as_nanos_u64(&self) -> u64 {
        self.as_nanos().try_into().unwrap_or(u64::MAX)
    }
}

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
    DbReadCacheInboxStats,
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
    /// Number of distinct lock levels.
    pub const COUNT: usize = 19;

    /// All lock levels in rank order (for iteration/snapshots).
    pub const ALL: [Self; Self::COUNT] = [
        Self::DbPoolCache,
        Self::DbSqliteInitGates,
        Self::DbReadCacheProjectsBySlug,
        Self::DbReadCacheProjectsByHumanKey,
        Self::DbReadCacheAgentsByKey,
        Self::DbReadCacheAgentsById,
        Self::DbReadCacheInboxStats,
        Self::DbReadCacheDeferredTouches,
        Self::DbReadCacheLastTouchFlush,
        Self::DbQueryTrackerInner,
        Self::StorageArchiveLockMap,
        Self::StorageRepoCache,
        Self::StorageSignalDebounce,
        Self::StorageWbqDrainHandle,
        Self::StorageWbqStats,
        Self::StorageCommitQueue,
        Self::ToolsBridgedEnv,
        Self::ToolsToolMetrics,
        Self::ServerLiveDashboard,
    ];

    /// Dense ordinal index [0..COUNT) for array-based stats lookup.
    #[must_use]
    pub const fn ordinal(self) -> usize {
        match self {
            Self::DbPoolCache => 0,
            Self::DbSqliteInitGates => 1,
            Self::DbReadCacheProjectsBySlug => 2,
            Self::DbReadCacheProjectsByHumanKey => 3,
            Self::DbReadCacheAgentsByKey => 4,
            Self::DbReadCacheAgentsById => 5,
            Self::DbReadCacheInboxStats => 6,
            Self::DbReadCacheDeferredTouches => 7,
            Self::DbReadCacheLastTouchFlush => 8,
            Self::DbQueryTrackerInner => 9,
            Self::StorageArchiveLockMap => 10,
            Self::StorageRepoCache => 11,
            Self::StorageSignalDebounce => 12,
            Self::StorageWbqDrainHandle => 13,
            Self::StorageWbqStats => 14,
            Self::StorageCommitQueue => 15,
            Self::ToolsBridgedEnv => 16,
            Self::ToolsToolMetrics => 17,
            Self::ServerLiveDashboard => 18,
        }
    }

    /// Reverse mapping from ordinal back to `LockLevel`.
    #[must_use]
    pub const fn from_ordinal(ord: usize) -> Option<Self> {
        match ord {
            0 => Some(Self::DbPoolCache),
            1 => Some(Self::DbSqliteInitGates),
            2 => Some(Self::DbReadCacheProjectsBySlug),
            3 => Some(Self::DbReadCacheProjectsByHumanKey),
            4 => Some(Self::DbReadCacheAgentsByKey),
            5 => Some(Self::DbReadCacheAgentsById),
            6 => Some(Self::DbReadCacheInboxStats),
            7 => Some(Self::DbReadCacheDeferredTouches),
            8 => Some(Self::DbReadCacheLastTouchFlush),
            9 => Some(Self::DbQueryTrackerInner),
            10 => Some(Self::StorageArchiveLockMap),
            11 => Some(Self::StorageRepoCache),
            12 => Some(Self::StorageSignalDebounce),
            13 => Some(Self::StorageWbqDrainHandle),
            14 => Some(Self::StorageWbqStats),
            15 => Some(Self::StorageCommitQueue),
            16 => Some(Self::ToolsBridgedEnv),
            17 => Some(Self::ToolsToolMetrics),
            18 => Some(Self::ServerLiveDashboard),
            _ => None,
        }
    }

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
            Self::DbReadCacheInboxStats => 24,
            Self::DbReadCacheDeferredTouches => 25,
            Self::DbReadCacheLastTouchFlush => 26,
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

// =============================================================================
// Lock contention tracking
// =============================================================================

/// Per-lock-level contention statistics (lock-free atomics).
struct LockStats {
    acquire_count: AtomicU64,
    contended_count: AtomicU64,
    total_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    max_hold_ns: AtomicU64,
}

impl LockStats {
    const fn new() -> Self {
        Self {
            acquire_count: AtomicU64::new(0),
            contended_count: AtomicU64::new(0),
            total_wait_ns: AtomicU64::new(0),
            total_hold_ns: AtomicU64::new(0),
            max_wait_ns: AtomicU64::new(0),
            max_hold_ns: AtomicU64::new(0),
        }
    }

    #[inline]
    fn record_acquire(&self, contended: bool, wait_ns: u64) {
        self.acquire_count.fetch_add(1, Ordering::Relaxed);
        if contended {
            self.contended_count.fetch_add(1, Ordering::Relaxed);
            self.total_wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
            update_max(&self.max_wait_ns, wait_ns);
        }
    }

    #[inline]
    fn record_hold(&self, hold_ns: u64) {
        self.total_hold_ns.fetch_add(hold_ns, Ordering::Relaxed);
        update_max(&self.max_hold_ns, hold_ns);
    }

    fn reset(&self) {
        self.acquire_count.store(0, Ordering::Relaxed);
        self.contended_count.store(0, Ordering::Relaxed);
        self.total_wait_ns.store(0, Ordering::Relaxed);
        self.total_hold_ns.store(0, Ordering::Relaxed);
        self.max_wait_ns.store(0, Ordering::Relaxed);
        self.max_hold_ns.store(0, Ordering::Relaxed);
    }
}

/// Lock-free CAS loop to update an atomic max value.
#[inline]
fn update_max(target: &AtomicU64, candidate: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while candidate > current {
        match target.compare_exchange_weak(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

fn global_lock_stats() -> &'static [LockStats] {
    static STATS: std::sync::LazyLock<Vec<LockStats>> =
        std::sync::LazyLock::new(|| (0..LockLevel::COUNT).map(|_| LockStats::new()).collect());
    &STATS
}

/// Snapshot of contention metrics for a single lock level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockContentionEntry {
    /// Debug name of the lock level (e.g., `"DbPoolCache"`).
    pub lock_name: String,
    /// Hierarchy rank (lower = acquired first).
    pub rank: u16,
    /// Total number of successful acquisitions.
    pub acquire_count: u64,
    /// Number of acquisitions where `try_lock()` failed (i.e., lock was held).
    pub contended_count: u64,
    /// Cumulative nanoseconds spent waiting for contended acquires.
    pub total_wait_ns: u64,
    /// Cumulative nanoseconds the lock was held across all acquisitions.
    pub total_hold_ns: u64,
    /// Maximum single wait duration (ns).
    pub max_wait_ns: u64,
    /// Maximum single hold duration (ns).
    pub max_hold_ns: u64,
    /// `contended_count / acquire_count` (0.0 if no acquires).
    pub contention_ratio: f64,
}

/// Returns a snapshot of contention metrics for all lock levels.
///
/// Only includes levels that have been acquired at least once.
#[must_use]
pub fn lock_contention_snapshot() -> Vec<LockContentionEntry> {
    let stats = global_lock_stats();
    LockLevel::ALL
        .iter()
        .filter_map(|&level| {
            let s = &stats[level.ordinal()];
            let acquires = s.acquire_count.load(Ordering::Relaxed);
            if acquires == 0 {
                return None;
            }
            let contended = s.contended_count.load(Ordering::Relaxed);
            Some(LockContentionEntry {
                lock_name: format!("{level:?}"),
                rank: level.rank(),
                acquire_count: acquires,
                contended_count: contended,
                total_wait_ns: s.total_wait_ns.load(Ordering::Relaxed),
                total_hold_ns: s.total_hold_ns.load(Ordering::Relaxed),
                max_wait_ns: s.max_wait_ns.load(Ordering::Relaxed),
                max_hold_ns: s.max_hold_ns.load(Ordering::Relaxed),
                #[allow(clippy::cast_precision_loss)] // acceptable for ratio display
                contention_ratio: contended as f64 / acquires as f64,
            })
        })
        .collect()
}

/// Resets all lock contention counters to zero. Useful for test isolation.
pub fn lock_contention_reset() {
    let stats = global_lock_stats();
    for s in stats {
        s.reset();
    }
}

// =============================================================================
// Lock ordering enforcement
// =============================================================================

#[cfg(debug_assertions)]
thread_local! {
    static HELD_LOCKS: RefCell<Vec<LockLevel>> = const { RefCell::new(Vec::new()) };
}

#[inline]
#[allow(unused_variables)]
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
#[allow(unused_variables)]
fn did_acquire(level: LockLevel) {
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| held.borrow_mut().push(level));
}

#[inline]
#[allow(unused_variables)]
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
        let stats = &global_lock_stats()[self.level.ordinal()];

        // Fast path: try non-blocking acquire first.
        match self.inner.try_lock() {
            Ok(guard) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedMutexGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                // Slow path: contended — measure wait time.
                let start = Instant::now();
                let guard = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let wait_ns = start.elapsed().as_nanos_u64();
                let acquired_at = Instant::now();
                stats.record_acquire(true, wait_ns);
                did_acquire(self.level);
                OrderedMutexGuard {
                    level: self.level,
                    acquired_at,
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedMutexGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard: e.into_inner(),
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn try_lock(&self) -> Option<OrderedMutexGuard<'_, T>> {
        check_before_acquire(self.level);
        let guard = self.inner.try_lock().ok()?;
        let stats = &global_lock_stats()[self.level.ordinal()];
        stats.record_acquire(false, 0);
        did_acquire(self.level);
        Some(OrderedMutexGuard {
            level: self.level,
            acquired_at: Instant::now(),
            guard,
        })
    }
}

pub struct OrderedMutexGuard<'a, T> {
    level: LockLevel,
    acquired_at: Instant,
    guard: MutexGuard<'a, T>,
}

impl<T> Drop for OrderedMutexGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
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
        let stats = &global_lock_stats()[self.level.ordinal()];

        match self.inner.try_read() {
            Ok(guard) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockReadGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let start = Instant::now();
                let guard = self
                    .inner
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let wait_ns = start.elapsed().as_nanos_u64();
                let acquired_at = Instant::now();
                stats.record_acquire(true, wait_ns);
                did_acquire(self.level);
                OrderedRwLockReadGuard {
                    level: self.level,
                    acquired_at,
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockReadGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard: e.into_inner(),
                }
            }
        }
    }

    pub fn write(&self) -> OrderedRwLockWriteGuard<'_, T> {
        check_before_acquire(self.level);
        let stats = &global_lock_stats()[self.level.ordinal()];

        match self.inner.try_write() {
            Ok(guard) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockWriteGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let start = Instant::now();
                let guard = self
                    .inner
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let wait_ns = start.elapsed().as_nanos_u64();
                let acquired_at = Instant::now();
                stats.record_acquire(true, wait_ns);
                did_acquire(self.level);
                OrderedRwLockWriteGuard {
                    level: self.level,
                    acquired_at,
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockWriteGuard {
                    level: self.level,
                    acquired_at: Instant::now(),
                    guard: e.into_inner(),
                }
            }
        }
    }
}

pub struct OrderedRwLockReadGuard<'a, T> {
    level: LockLevel,
    acquired_at: Instant,
    guard: RwLockReadGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
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
    acquired_at: Instant,
    guard: RwLockWriteGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
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
    #[cfg(debug_assertions)]
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

    // -----------------------------------------------------------------------
    // Lock level enumeration
    // -----------------------------------------------------------------------

    #[test]
    fn lock_level_all_length_matches_count() {
        assert_eq!(LockLevel::ALL.len(), LockLevel::COUNT);
    }

    #[test]
    fn lock_level_ordinal_roundtrip() {
        for (i, &level) in LockLevel::ALL.iter().enumerate() {
            assert_eq!(level.ordinal(), i, "ordinal mismatch for {level:?}");
            assert_eq!(
                LockLevel::from_ordinal(i),
                Some(level),
                "from_ordinal mismatch for ordinal {i}"
            );
        }
        assert_eq!(LockLevel::from_ordinal(LockLevel::COUNT), None);
    }

    #[test]
    fn lock_level_all_in_rank_order() {
        for w in LockLevel::ALL.windows(2) {
            assert!(
                w[0].rank() < w[1].rank(),
                "{:?}@{} should precede {:?}@{}",
                w[0],
                w[0].rank(),
                w[1],
                w[1].rank()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Contention tracking: basic
    //
    // Note: global lock stats are process-wide, so parallel tests can
    // interfere. Tests use baseline readings and check deltas.
    // -----------------------------------------------------------------------

    fn stats_for(level: LockLevel) -> (u64, u64, u64, u64) {
        let s = &global_lock_stats()[level.ordinal()];
        (
            s.acquire_count.load(Ordering::Relaxed),
            s.contended_count.load(Ordering::Relaxed),
            s.total_hold_ns.load(Ordering::Relaxed),
            s.max_hold_ns.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn contention_snapshot_tracks_uncontended_acquire() {
        let level = LockLevel::DbPoolCache;
        let (base_acq, base_cont, base_hold, _) = stats_for(level);
        let m = OrderedMutex::new(level, 42u32);
        {
            let g = m.lock();
            assert_eq!(*g, 42);
            drop(g);
        }
        let (acq, cont, hold, _) = stats_for(level);
        assert!(acq > base_acq, "acquire_count didn't increase");
        assert_eq!(cont, base_cont, "should have 0 new contention events");
        assert!(hold > base_hold, "hold_ns should have increased");
    }

    #[test]
    fn contention_snapshot_tracks_try_lock() {
        let level = LockLevel::DbSqliteInitGates;
        let (base_acq, base_cont, _, _) = stats_for(level);
        let m = OrderedMutex::new(level, ());
        {
            let _g = m.try_lock().expect("should succeed");
        }
        let (acq, cont, _, _) = stats_for(level);
        assert!(acq > base_acq, "acquire_count didn't increase");
        assert_eq!(cont, base_cont, "try_lock success should not be contended");
    }

    #[test]
    fn contention_snapshot_filters_zero_levels() {
        // Verify that lock_contention_snapshot() excludes levels with 0 acquires.
        let snap = lock_contention_snapshot();
        for entry in &snap {
            assert!(
                entry.acquire_count > 0,
                "zero-acquire entry should be filtered: {}",
                entry.lock_name
            );
        }
    }

    #[test]
    fn contention_reset_zeros_single_level() {
        // Verify that LockStats::reset() works on a single level.
        let level = LockLevel::ToolsBridgedEnv;
        let m = OrderedMutex::new(level, ());
        {
            let _g = m.lock();
        }
        let s = &global_lock_stats()[level.ordinal()];
        assert!(s.acquire_count.load(Ordering::Relaxed) > 0);
        s.reset();
        assert_eq!(s.acquire_count.load(Ordering::Relaxed), 0);
        assert_eq!(s.contended_count.load(Ordering::Relaxed), 0);
        assert_eq!(s.total_hold_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn contention_global_reset() {
        // Verify lock_contention_reset() runs without panic.
        lock_contention_reset();
        // Snapshot after reset should be empty or near-empty.
        let snap = lock_contention_snapshot();
        // Parallel tests may have re-acquired, so just verify it didn't crash.
        assert!(snap.len() <= LockLevel::COUNT);
    }

    // -----------------------------------------------------------------------
    // Contention tracking: contended path
    // -----------------------------------------------------------------------

    #[test]
    fn contention_detected_under_contention() {
        // Verify that multi-threaded contention produces correct data
        // and that contention tracking doesn't corrupt the protected value.
        let m = Arc::new(OrderedMutex::new(LockLevel::StorageCommitQueue, 0u64));
        let iterations: u64 = 50;
        let threads: u64 = 4;

        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let m = Arc::clone(&m);
                thread::spawn(move || {
                    for _ in 0..iterations {
                        let mut g = m.lock();
                        *g += 1;
                        drop(g);
                        // Yield briefly to increase contention probability.
                        thread::yield_now();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // The protected value must be exactly correct — proves the lock works.
        assert_eq!(*m.lock(), threads * iterations);

        // Stats should show some acquires (exact count depends on test ordering
        // and parallel resets, so we just verify > 0).
        let snap = lock_contention_snapshot();
        let entry = snap.iter().find(|e| e.lock_name == "StorageCommitQueue");
        // Entry might be missing if another test reset stats, but if present
        // it should have reasonable values.
        if let Some(entry) = entry {
            assert!(entry.acquire_count > 0);
            assert!(
                entry.contention_ratio >= 0.0 && entry.contention_ratio <= 1.0,
                "contention_ratio {} out of range",
                entry.contention_ratio
            );
        }
    }

    // -----------------------------------------------------------------------
    // Contention tracking: RwLock
    // -----------------------------------------------------------------------

    #[test]
    fn rwlock_contention_tracking() {
        lock_contention_reset();
        let rw = OrderedRwLock::new(LockLevel::ServerLiveDashboard, 0u64);
        // Multiple reads should be uncontended with each other.
        {
            let _r1 = rw.read();
        }
        {
            let _r2 = rw.read();
        }
        // One write.
        {
            let mut w = rw.write();
            *w = 42;
        }
        let snap = lock_contention_snapshot();
        let entry = snap
            .iter()
            .find(|e| e.lock_name == "ServerLiveDashboard")
            .expect("should have entry");
        // 2 reads + 1 write = 3 acquires.
        assert!(
            entry.acquire_count >= 3,
            "acquire_count {} < 3",
            entry.acquire_count
        );
    }

    // -----------------------------------------------------------------------
    // LockContentionEntry serialization
    // -----------------------------------------------------------------------

    #[test]
    fn lock_contention_entry_serializes() {
        let entry = LockContentionEntry {
            lock_name: "DbPoolCache".to_string(),
            rank: 10,
            acquire_count: 100,
            contended_count: 5,
            total_wait_ns: 1_000_000,
            total_hold_ns: 50_000_000,
            max_wait_ns: 500_000,
            max_hold_ns: 2_000_000,
            contention_ratio: 0.05,
        };
        let json = serde_json::to_string(&entry).expect("should serialize");
        assert!(json.contains("\"lock_name\":\"DbPoolCache\""));
        assert!(json.contains("\"contention_ratio\":0.05"));

        let roundtrip: LockContentionEntry =
            serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(roundtrip.acquire_count, 100);
        assert_eq!(roundtrip.contended_count, 5);
    }

    // -----------------------------------------------------------------------
    // update_max helper
    // -----------------------------------------------------------------------

    #[test]
    fn update_max_works_correctly() {
        let a = AtomicU64::new(10);
        update_max(&a, 5); // should not change
        assert_eq!(a.load(Ordering::Relaxed), 10);
        update_max(&a, 20); // should update
        assert_eq!(a.load(Ordering::Relaxed), 20);
        update_max(&a, 20); // equal — should not change
        assert_eq!(a.load(Ordering::Relaxed), 20);
        update_max(&a, 100); // should update
        assert_eq!(a.load(Ordering::Relaxed), 100);
    }

    // -----------------------------------------------------------------------
    // Additional edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn lock_level_display_includes_rank() {
        let level = LockLevel::DbPoolCache;
        let display = format!("{level}");
        assert_eq!(display, "DbPoolCache@10");

        let level = LockLevel::ServerLiveDashboard;
        let display = format!("{level}");
        assert_eq!(display, "ServerLiveDashboard@90");
    }

    #[test]
    fn lock_level_all_ranks_are_unique() {
        let mut ranks: Vec<u16> = LockLevel::ALL.iter().map(|l| l.rank()).collect();
        let original_len = ranks.len();
        ranks.sort_unstable();
        ranks.dedup();
        assert_eq!(
            ranks.len(),
            original_len,
            "all lock levels should have unique ranks"
        );
    }

    #[test]
    fn lock_level_from_ordinal_out_of_bounds() {
        assert_eq!(LockLevel::from_ordinal(LockLevel::COUNT), None);
        assert_eq!(LockLevel::from_ordinal(999), None);
        assert_eq!(LockLevel::from_ordinal(usize::MAX), None);
    }

    #[test]
    fn ordered_mutex_level_getter() {
        let m = OrderedMutex::new(LockLevel::DbPoolCache, ());
        assert_eq!(m.level(), LockLevel::DbPoolCache);
    }

    #[test]
    fn ordered_rwlock_level_getter() {
        let rw = OrderedRwLock::new(LockLevel::StorageRepoCache, ());
        assert_eq!(rw.level(), LockLevel::StorageRepoCache);
    }

    #[test]
    fn ordered_mutex_deref_read_write() {
        let m = OrderedMutex::new(LockLevel::ToolsToolMetrics, vec![1, 2, 3]);
        {
            let guard = m.lock();
            // Deref: read through guard
            assert_eq!(guard.len(), 3);
            assert_eq!(guard[0], 1);
            drop(guard);
        }
        {
            let mut guard = m.lock();
            // DerefMut: write through guard
            guard.push(4);
            assert_eq!(guard.len(), 4);
            drop(guard);
        }
    }

    #[test]
    fn ordered_rwlock_deref_read_write() {
        let rw = OrderedRwLock::new(LockLevel::StorageSignalDebounce, String::from("hello"));
        {
            let guard = rw.read();
            assert_eq!(guard.as_str(), "hello");
            drop(guard);
        }
        {
            let mut guard = rw.write();
            guard.push_str(" world");
            drop(guard);
        }
        {
            let guard = rw.read();
            assert_eq!(guard.as_str(), "hello world");
            drop(guard);
        }
    }

    #[test]
    fn try_lock_fails_when_held() {
        let m = OrderedMutex::new(LockLevel::StorageWbqDrainHandle, ());
        let _guard = m.lock();
        // try_lock from the same thread should fail (mutex is already held).
        // Note: On some platforms try_lock from the same thread may succeed (re-entrant),
        // but std::sync::Mutex is not re-entrant, so this should return None.
        // However, in debug builds, check_before_acquire will panic on same-level re-entry.
        // In release builds, try_lock just returns None.
        #[cfg(not(debug_assertions))]
        {
            assert!(
                m.try_lock().is_none(),
                "try_lock should fail when mutex is already held"
            );
        }
    }

    #[test]
    fn lock_contention_entry_debug() {
        let entry = LockContentionEntry {
            lock_name: "TestLock".to_string(),
            rank: 99,
            acquire_count: 50,
            contended_count: 3,
            total_wait_ns: 100_000,
            total_hold_ns: 500_000,
            max_wait_ns: 50_000,
            max_hold_ns: 100_000,
            contention_ratio: 0.06,
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("TestLock"));
        assert!(debug.contains("50"));

        let cloned = entry.clone();
        assert_eq!(cloned.lock_name, "TestLock");
        assert_eq!(cloned.rank, 99);
        // Use `entry` after clone to prove independent copy.
        assert_eq!(entry.lock_name, "TestLock");
    }

    #[test]
    fn lock_level_copy_clone_eq() {
        let a = LockLevel::DbPoolCache;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a; // Copy (also works as Clone)
        assert_eq!(a, c);
        assert_ne!(a, LockLevel::ServerLiveDashboard);
    }

    #[test]
    fn duration_nanos_u64_edge_cases() {
        let zero = std::time::Duration::ZERO;
        assert_eq!(zero.as_nanos_u64(), 0);

        let one_sec = std::time::Duration::from_secs(1);
        assert_eq!(one_sec.as_nanos_u64(), 1_000_000_000);

        let max = std::time::Duration::MAX;
        // Should saturate to u64::MAX rather than panic.
        assert_eq!(max.as_nanos_u64(), u64::MAX);
    }
}
