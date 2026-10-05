//! Lock ordering, debug-only deadlock prevention, and live contention diagnostics.
//!
//! This module defines a **global lock hierarchy** for the small set of
//! process-global locks that may be acquired across subsystems (db/storage/tools).
//! At extreme concurrency, a single inconsistent acquisition order can deadlock
//! the entire process.
//!
//! Design goals:
//! - **Debug-only ordering checks**: ordering checks compile to no-ops outside
//!   `debug_assertions`.
//! - **Fail fast in debug**: panic *before* attempting an out-of-order lock.
//! - **Incremental adoption**: wrap only the locks that matter.
//! - **Contention visibility**: completed-acquisition counters plus current
//!   holders and blocked waiters, including their thread identities and sites.
//!   Metadata is per instance, not a new global hot-path lock. Release overhead
//!   must be measured; the historical atomic-counter-only estimate no longer
//!   describes the live tracking path.
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
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use serde::{Deserialize, Serialize};

mod live;
use live::{ActivityGuard, LiveLock};
pub use live::{
    LockAccess, LockActivity, LockActivitySnapshot, LockParticipant, lock_activity_snapshot,
};

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
        // A completed hold is a real event even when the platform clock's
        // resolution reports a zero-length interval. Preserve that invariant
        // instead of making the aggregate indistinguishable from no holds.
        let hold_ns = hold_ns.max(1);
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
    /// This level's live instances from one shared, nonblocking observation.
    /// Coverage flags describe the entire registry sample; a partial sample is
    /// not evidence that this level has no blockers. Historical reports that
    /// lack this field deserialize as unavailable, never verified-empty.
    #[serde(default)]
    pub live: LockActivitySnapshot,
}

/// Returns a snapshot of contention metrics for all lock levels.
///
/// Includes used levels and levels with observed activity after a counter reset.
/// If live coverage is incomplete, retain the coverage flags even for levels
/// with no completed acquisitions. Waits need not finish before appearing here.
#[must_use]
pub fn lock_contention_snapshot() -> Vec<LockContentionEntry> {
    let stats = global_lock_stats();
    let live = lock_activity_snapshot();
    LockLevel::ALL
        .iter()
        .filter_map(|&level| {
            let s = &stats[level.ordinal()];
            let acquires = s.acquire_count.load(Ordering::Relaxed);
            let activity = LockActivitySnapshot {
                available: live.available,
                locks: live
                    .locks
                    .iter()
                    .filter(|lock| lock.rank == level.rank())
                    .cloned()
                    .collect(),
                busy_instances: live.busy_instances,
                omitted_active_instances: live.omitted_active_instances,
                registrations_dropped: live.registrations_dropped,
            };
            if acquires == 0 && activity.locks.is_empty() && activity.is_complete() {
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
                contention_ratio: if acquires == 0 {
                    0.0
                } else {
                    contended as f64 / acquires as f64
                },
                live: activity,
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
    static HELD_LOCKS: RefCell<Vec<(LockLevel, &'static Location<'static>)>> =
        const { RefCell::new(Vec::new()) };
}

#[inline]
#[track_caller]
#[allow(unused_variables)]
fn check_before_acquire(level: LockLevel) {
    #[cfg(debug_assertions)]
    let site = Location::caller();
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| {
        let held = held.borrow();
        let Some(&(last, acquired_at)) = held.last() else {
            return;
        };
        assert!(
            level.rank() > last.rank(),
            "lock order violation: attempting to acquire {} at {} while holding {} acquired at {}. held={:?}",
            level,
            site,
            last,
            acquired_at,
            held.as_slice()
        );
    });
}

#[inline]
#[track_caller]
#[allow(unused_variables)]
fn did_acquire(level: LockLevel) {
    #[cfg(debug_assertions)]
    let site = Location::caller();
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| held.borrow_mut().push((level, site)));
}

#[inline]
#[allow(unused_variables)]
fn did_release(level: LockLevel) {
    #[cfg(debug_assertions)]
    HELD_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        if let Some(pos) = held
            .iter()
            .rposition(|&(held_level, _)| held_level == level)
        {
            held.remove(pos);
        } else {
            panic!(
                "lock tracking corrupted: expected to release {}, but it was not held. held={:?}",
                level,
                held.as_slice()
            );
        }
    });
}

/// Mutex wrapper that enforces the global lock hierarchy in debug builds.
#[derive(Debug)]
pub struct OrderedMutex<T> {
    level: LockLevel,
    inner: Mutex<T>,
    activity: LiveLock,
}

impl<T> OrderedMutex<T> {
    #[must_use]
    pub const fn new(level: LockLevel, value: T) -> Self {
        Self {
            level,
            inner: Mutex::new(value),
            activity: LiveLock::new(),
        }
    }

    #[must_use]
    pub const fn level(&self) -> LockLevel {
        self.level
    }

    #[track_caller]
    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        check_before_acquire(self.level);
        let stats = &global_lock_stats()[self.level.ordinal()];

        // Fast path: try non-blocking acquire first.
        match self.inner.try_lock() {
            Ok(guard) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Mutex,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedMutexGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                // Slow path: contended — measure wait time.
                let start = Instant::now();
                let waiting =
                    self.activity
                        .waiting(self.level, LockAccess::Mutex, Location::caller(), start);
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
                    activity: waiting.map(|waiting| waiting.acquired(acquired_at)),
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Mutex,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedMutexGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard: e.into_inner(),
                }
            }
        }
    }

    #[allow(dead_code)]
    #[track_caller]
    pub fn try_lock(&self) -> Option<OrderedMutexGuard<'_, T>> {
        check_before_acquire(self.level);
        let guard = self.inner.try_lock().ok()?;
        let acquired_at = Instant::now();
        let activity = self.activity.holding(
            self.level,
            LockAccess::Mutex,
            Location::caller(),
            acquired_at,
        );
        let stats = &global_lock_stats()[self.level.ordinal()];
        stats.record_acquire(false, 0);
        did_acquire(self.level);
        Some(OrderedMutexGuard {
            level: self.level,
            acquired_at,
            activity,
            guard,
        })
    }
}

pub struct OrderedMutexGuard<'a, T> {
    level: LockLevel,
    acquired_at: Instant,
    activity: Option<ActivityGuard>,
    guard: MutexGuard<'a, T>,
}

impl<T> Drop for OrderedMutexGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
        did_release(self.level);
        // Remove this unique observation before unlocking the application
        // mutex. A subsequent holder cannot be cleared by this guard's drop.
        drop(self.activity.take());
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
    activity: LiveLock,
}

impl<T> OrderedRwLock<T> {
    #[must_use]
    pub const fn new(level: LockLevel, value: T) -> Self {
        Self {
            level,
            inner: RwLock::new(value),
            activity: LiveLock::new(),
        }
    }

    #[must_use]
    pub const fn level(&self) -> LockLevel {
        self.level
    }

    #[track_caller]
    pub fn read(&self) -> OrderedRwLockReadGuard<'_, T> {
        check_before_acquire(self.level);
        let stats = &global_lock_stats()[self.level.ordinal()];

        match self.inner.try_read() {
            Ok(guard) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Read,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockReadGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let start = Instant::now();
                let waiting =
                    self.activity
                        .waiting(self.level, LockAccess::Read, Location::caller(), start);
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
                    activity: waiting.map(|waiting| waiting.acquired(acquired_at)),
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Read,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockReadGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard: e.into_inner(),
                }
            }
        }
    }

    #[track_caller]
    pub fn write(&self) -> OrderedRwLockWriteGuard<'_, T> {
        check_before_acquire(self.level);
        let stats = &global_lock_stats()[self.level.ordinal()];

        match self.inner.try_write() {
            Ok(guard) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Write,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockWriteGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard,
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let start = Instant::now();
                let waiting =
                    self.activity
                        .waiting(self.level, LockAccess::Write, Location::caller(), start);
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
                    activity: waiting.map(|waiting| waiting.acquired(acquired_at)),
                    guard,
                }
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                let acquired_at = Instant::now();
                let activity = self.activity.holding(
                    self.level,
                    LockAccess::Write,
                    Location::caller(),
                    acquired_at,
                );
                stats.record_acquire(false, 0);
                did_acquire(self.level);
                OrderedRwLockWriteGuard {
                    level: self.level,
                    acquired_at,
                    activity,
                    guard: e.into_inner(),
                }
            }
        }
    }
}

pub struct OrderedRwLockReadGuard<'a, T> {
    level: LockLevel,
    acquired_at: Instant,
    activity: Option<ActivityGuard>,
    guard: RwLockReadGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
        did_release(self.level);
        drop(self.activity.take());
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
    activity: Option<ActivityGuard>,
    guard: RwLockWriteGuard<'a, T>,
}

impl<T> Drop for OrderedRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let hold_ns = self.acquired_at.elapsed().as_nanos_u64();
        global_lock_stats()[self.level.ordinal()].record_hold(hold_ns);
        did_release(self.level);
        drop(self.activity.take());
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

    static CONTENTION_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn contention_test_guard() -> std::sync::MutexGuard<'static, ()> {
        CONTENTION_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        let _stats_guard = contention_test_guard();
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
    // Global lock stats are process-wide. Tests that inspect or reset them use
    // `contention_test_guard()` so a reset cannot invalidate another test's
    // baseline while it is making assertions.
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
        let _stats_guard = contention_test_guard();
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
        let _stats_guard = contention_test_guard();
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
    fn contention_snapshot_filters_only_verified_idle_zero_levels() {
        let _stats_guard = contention_test_guard();
        // Live waits survive counter reset. Missing coverage must stay visible
        // instead of being converted into an apparently idle lock level.
        let snap = lock_contention_snapshot();
        for entry in &snap {
            assert!(
                entry.acquire_count > 0
                    || !entry.live.locks.is_empty()
                    || !entry.live.is_complete(),
                "verified-idle zero-acquire entry should be filtered: {}",
                entry.lock_name
            );
        }
    }

    #[test]
    fn contention_reset_zeros_single_level() {
        let _stats_guard = contention_test_guard();
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
        let _stats_guard = contention_test_guard();
        // Verify lock_contention_reset() runs without panic.
        lock_contention_reset();
        // Snapshot after reset should be empty or near-empty.
        let snap = lock_contention_snapshot();
        // Parallel tests may have re-acquired, so just verify it didn't crash.
        assert!(snap.len() <= LockLevel::COUNT);
    }

    #[test]
    fn diagnostic_report_exposes_an_unfinished_wait_after_counter_reset() {
        use std::sync::mpsc;

        let _stats_guard = contention_test_guard();
        let level = LockLevel::ServerLiveDashboard;
        let mutex = Arc::new(OrderedMutex::new(level, "not diagnostic content"));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held = Arc::clone(&mutex);
        let owner = thread::Builder::new()
            .name("diagnostic-owner".into())
            .spawn(move || {
                let guard = held.lock();
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
                drop(guard);
            })
            .unwrap();
        let ready = ready_rx.recv_timeout(Duration::from_secs(5));
        // Live leases are not historical counters. Resetting counters cannot
        // erase an existing owner or make its blocked contender disappear.
        global_lock_stats()[level.ordinal()].reset();
        let held = Arc::clone(&mutex);
        let waiter = thread::Builder::new()
            .name("diagnostic-waiter".into())
            .spawn(move || drop(held.lock()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            // This is the same builder/serializer used by the real
            // resource://tooling/diagnostics handler, not a synthetic report.
            let report = crate::diagnostics::DiagnosticReport::build(vec![], vec![]);
            let payload: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
            let found = payload["locks"].as_array().and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry["lock_name"] == "ServerLiveDashboard"
                            && entry["live"]["locks"].as_array().is_some_and(|locks| {
                                locks.iter().any(|lock| {
                                    lock["waiters"].as_array().is_some_and(|waiters| {
                                        waiters.iter().any(|waiter| {
                                            waiter["thread_name"] == "diagnostic-waiter"
                                        })
                                    })
                                })
                            })
                    })
                    .cloned()
            });
            if found.is_some() || Instant::now() >= deadline {
                break found;
            }
            thread::sleep(Duration::from_millis(1));
        };
        let _ = release_tx.send(());
        owner.join().unwrap();
        waiter.join().unwrap();
        ready.unwrap();
        let captured = captured.expect("diagnostics must expose the wait before it completes");
        assert_eq!(captured["acquire_count"], 0);
        assert_eq!(captured["contention_ratio"], 0.0);
        assert_eq!(captured["live"]["available"], true);
        let locks = captured["live"]["locks"].as_array().unwrap();
        let instance = locks
            .iter()
            .find(|lock| {
                lock["holders"].as_array().is_some_and(|holders| {
                    holders
                        .iter()
                        .any(|holder| holder["thread_name"] == "diagnostic-owner")
                })
            })
            .unwrap();
        assert_eq!(instance["holder_count"], 1);
        assert_eq!(instance["waiter_count"], 1);
        assert!(instance["oldest_observed_wait_ns"].as_u64().unwrap() > 0);
        assert!(!captured.to_string().contains("not diagnostic content"));
    }

    #[test]
    fn same_rank_lock_instances_are_not_merged_in_diagnostics() {
        use std::sync::mpsc;

        let first = Arc::new(OrderedMutex::new(LockLevel::DbReadCacheInboxStats, 100));
        let second = Arc::new(OrderedMutex::new(LockLevel::DbReadCacheInboxStats, 200));
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut owners = Vec::new();
        let mut releases = Vec::new();
        for (name, mutex) in [
            ("shard-a", Arc::clone(&first)),
            ("shard-b", Arc::clone(&second)),
        ] {
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            let ready_tx = ready_tx.clone();
            owners.push(
                thread::Builder::new()
                    .name(name.into())
                    .spawn(move || {
                        let guard = mutex.lock();
                        ready_tx.send(()).unwrap();
                        let _ = release_rx.recv_timeout(Duration::from_secs(10));
                        drop(guard);
                    })
                    .unwrap(),
            );
        }
        let ready_a = ready_rx.recv_timeout(Duration::from_secs(5));
        let ready_b = ready_rx.recv_timeout(Duration::from_secs(5));
        let waiting = Arc::clone(&first);
        let waiter = thread::Builder::new()
            .name("shard-a-waiter".into())
            .spawn(move || *waiting.lock() += 1)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let observed = loop {
            let locks: Vec<_> = lock_contention_snapshot()
                .into_iter()
                .flat_map(|level| level.live.locks)
                .filter(|lock| {
                    lock.holders.iter().any(|holder| {
                        matches!(holder.thread_name.as_deref(), Some("shard-a" | "shard-b"))
                    })
                })
                .collect();
            if observed_shards_ready(&locks) || Instant::now() >= deadline {
                break locks;
            }
            thread::sleep(Duration::from_millis(1));
        };
        for release in releases {
            let _ = release.send(());
        }
        for owner in owners {
            owner.join().unwrap();
        }
        waiter.join().unwrap();
        ready_a.unwrap();
        ready_b.unwrap();
        assert!(observed_shards_ready(&observed));
        assert_ne!(observed[0].instance_id, observed[1].instance_id);
        assert_eq!(*first.lock(), 101);
        assert_eq!(*second.lock(), 200);
    }

    fn observed_shards_ready(locks: &[LockActivity]) -> bool {
        locks.len() == 2
            && locks.iter().any(|lock| {
                lock.holders
                    .iter()
                    .any(|holder| holder.thread_name.as_deref() == Some("shard-a"))
                    && lock
                        .waiters
                        .iter()
                        .any(|waiter| waiter.thread_name.as_deref() == Some("shard-a-waiter"))
            })
            && locks.iter().any(|lock| {
                lock.holders
                    .iter()
                    .any(|holder| holder.thread_name.as_deref() == Some("shard-b"))
                    && lock.waiter_count == 0
            })
    }

    #[test]
    fn historical_lock_payload_has_unavailable_live_evidence() {
        let payload = serde_json::json!({
            "lock_name": "DbPoolCache", "rank": 10, "acquire_count": 1,
            "contended_count": 0, "total_wait_ns": 0, "total_hold_ns": 1,
            "max_wait_ns": 0, "max_hold_ns": 1, "contention_ratio": 0.0,
        });
        let entry: LockContentionEntry = serde_json::from_value(payload).unwrap();
        assert!(!entry.live.available);
        assert!(!entry.live.is_complete());
        assert_eq!(entry.live.locks, [] as [LockActivity; 0]);
    }

    // -----------------------------------------------------------------------
    // Contention tracking: contended path
    // -----------------------------------------------------------------------

    #[test]
    fn contention_detected_under_contention() {
        let _stats_guard = contention_test_guard();
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
        let _stats_guard = contention_test_guard();
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
            live: LockActivitySnapshot::default(),
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
            live: LockActivitySnapshot::default(),
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
