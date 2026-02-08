//! Lock-free metrics primitives + a small global metrics surface.
//!
//! Design goals:
//! - Hot-path recording: O(1), no allocations, no locks.
//! - Snapshotting: lock-free loads + derived quantiles (approx) for histograms.
//!
//! This is intentionally lightweight (std-only) so all crates can record metrics.

#![forbid(unsafe_code)]

use serde::Serialize;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Counter {
    v: AtomicU64,
}

impl Counter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            v: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn inc(&self) {
        self.v.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, delta: u64) {
        self.v.fetch_add(delta, Ordering::Relaxed);
    }

    #[inline]
    pub fn load(&self) -> u64 {
        self.v.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn store(&self, value: u64) {
        self.v.store(value, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub struct GaugeI64 {
    v: AtomicI64,
}

impl GaugeI64 {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            v: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn add(&self, delta: i64) {
        self.v.fetch_add(delta, Ordering::Relaxed);
    }

    #[inline]
    pub fn set(&self, value: i64) {
        self.v.store(value, Ordering::Relaxed);
    }

    #[inline]
    pub fn load(&self) -> i64 {
        self.v.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct GaugeU64 {
    v: AtomicU64,
}

impl GaugeU64 {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            v: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn add(&self, delta: u64) {
        self.v.fetch_add(delta, Ordering::Relaxed);
    }

    #[inline]
    pub fn set(&self, value: u64) {
        self.v.store(value, Ordering::Relaxed);
    }

    #[inline]
    pub fn load(&self) -> u64 {
        self.v.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn fetch_max(&self, value: u64) {
        let mut cur = self.v.load(Ordering::Relaxed);
        while value > cur {
            match self
                .v
                .compare_exchange_weak(cur, value, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(next) => cur = next,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Histogram (fixed-bucket log2)
// ---------------------------------------------------------------------------

const LOG2_BUCKETS: usize = 64;

#[derive(Debug)]
pub struct Log2Histogram {
    buckets: [AtomicU64; LOG2_BUCKETS],
    count: AtomicU64,
    sum: AtomicU64,
    min: AtomicU64,
    max: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
}

impl Default for Log2Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Log2Histogram {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record(&self, value: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.min.fetch_min(value, Ordering::Relaxed);
        self.max.fetch_max(value, Ordering::Relaxed);
        let idx = bucket_index(value);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Reset all counters to their initial state.
    pub fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.sum.store(0, Ordering::Relaxed);
        self.min.store(u64::MAX, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return HistogramSnapshot {
                count: 0,
                sum: 0,
                min: 0,
                max: 0,
                p50: 0,
                p95: 0,
                p99: 0,
            };
        }

        let buckets: [u64; LOG2_BUCKETS] =
            std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed));

        let max = self.max.load(Ordering::Relaxed);
        let p50 = estimate_quantile_frac(&buckets, count, 1, 2, max);
        let p95 = estimate_quantile_frac(&buckets, count, 19, 20, max);
        let p99 = estimate_quantile_frac(&buckets, count, 99, 100, max);

        HistogramSnapshot {
            count,
            sum: self.sum.load(Ordering::Relaxed),
            min: self.min.load(Ordering::Relaxed),
            max,
            p50,
            p95,
            p99,
        }
    }
}

#[inline]
const fn bucket_index(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    let lz = value.leading_zeros() as usize;
    // floor(log2(value)) in range 0..=63
    63usize.saturating_sub(lz)
}

const fn bucket_upper_bound(idx: usize) -> u64 {
    if idx >= 63 {
        return u64::MAX;
    }
    (1u64 << (idx + 1)).saturating_sub(1)
}

fn estimate_quantile_frac(
    buckets: &[u64; LOG2_BUCKETS],
    count: u64,
    numerator: u64,
    denominator: u64,
    observed_max: u64,
) -> u64 {
    debug_assert!(denominator > 0);
    // Nearest-rank method: smallest value x such that F(x) >= q.
    // rank is 1-indexed, clamp to [1, count]
    let numerator = numerator.min(denominator);
    let mut rank = count
        .saturating_mul(numerator)
        .saturating_add(denominator.saturating_sub(1))
        / denominator;
    rank = rank.clamp(1, count);

    let mut cumulative = 0u64;
    for (idx, c) in buckets.iter().copied().enumerate() {
        cumulative = cumulative.saturating_add(c);
        if cumulative >= rank {
            return bucket_upper_bound(idx).min(observed_max);
        }
    }
    // Should not happen unless counts race snapshot; return max as conservative fallback.
    observed_max
}

// ---------------------------------------------------------------------------
// Global metrics surface (minimal; expanded by dedicated beads).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HttpMetrics {
    pub requests_total: Counter,
    pub requests_inflight: GaugeI64,
    pub requests_2xx: Counter,
    pub requests_4xx: Counter,
    pub requests_5xx: Counter,
    pub latency_us: Log2Histogram,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpMetricsSnapshot {
    pub requests_total: u64,
    pub requests_inflight: i64,
    pub requests_2xx: u64,
    pub requests_4xx: u64,
    pub requests_5xx: u64,
    pub latency_us: HistogramSnapshot,
}

impl Default for HttpMetrics {
    fn default() -> Self {
        Self {
            requests_total: Counter::new(),
            requests_inflight: GaugeI64::new(),
            requests_2xx: Counter::new(),
            requests_4xx: Counter::new(),
            requests_5xx: Counter::new(),
            latency_us: Log2Histogram::new(),
        }
    }
}

impl HttpMetrics {
    #[inline]
    pub fn record_response(&self, status: u16, latency_us: u64) {
        self.requests_total.inc();
        match status {
            200..=299 => self.requests_2xx.inc(),
            400..=499 => self.requests_4xx.inc(),
            500..=599 => self.requests_5xx.inc(),
            _ => {}
        }
        self.latency_us.record(latency_us);
    }

    #[must_use]
    pub fn snapshot(&self) -> HttpMetricsSnapshot {
        HttpMetricsSnapshot {
            requests_total: self.requests_total.load(),
            requests_inflight: self.requests_inflight.load(),
            requests_2xx: self.requests_2xx.load(),
            requests_4xx: self.requests_4xx.load(),
            requests_5xx: self.requests_5xx.load(),
            latency_us: self.latency_us.snapshot(),
        }
    }
}

#[derive(Debug)]
pub struct ToolsMetrics {
    pub tool_calls_total: Counter,
    pub tool_errors_total: Counter,
    pub tool_latency_us: Log2Histogram,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsMetricsSnapshot {
    pub tool_calls_total: u64,
    pub tool_errors_total: u64,
    pub tool_latency_us: HistogramSnapshot,
}

impl Default for ToolsMetrics {
    fn default() -> Self {
        Self {
            tool_calls_total: Counter::new(),
            tool_errors_total: Counter::new(),
            tool_latency_us: Log2Histogram::new(),
        }
    }
}

impl ToolsMetrics {
    #[inline]
    pub fn record_call(&self, latency_us: u64, is_error: bool) {
        self.tool_calls_total.inc();
        if is_error {
            self.tool_errors_total.inc();
        }
        self.tool_latency_us.record(latency_us);
    }

    #[must_use]
    pub fn snapshot(&self) -> ToolsMetricsSnapshot {
        ToolsMetricsSnapshot {
            tool_calls_total: self.tool_calls_total.load(),
            tool_errors_total: self.tool_errors_total.load(),
            tool_latency_us: self.tool_latency_us.snapshot(),
        }
    }
}

#[derive(Debug)]
pub struct DbMetrics {
    pub pool_acquires_total: Counter,
    pub pool_acquire_latency_us: Log2Histogram,
    pub pool_acquire_errors_total: Counter,
    pub pool_total_connections: GaugeU64,
    pub pool_idle_connections: GaugeU64,
    pub pool_active_connections: GaugeU64,
    pub pool_pending_requests: GaugeU64,
    pub pool_peak_active_connections: GaugeU64,
    pub pool_over_80_since_us: GaugeU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbMetricsSnapshot {
    pub pool_acquires_total: u64,
    pub pool_acquire_errors_total: u64,
    pub pool_acquire_latency_us: HistogramSnapshot,
    pub pool_total_connections: u64,
    pub pool_idle_connections: u64,
    pub pool_active_connections: u64,
    pub pool_pending_requests: u64,
    pub pool_peak_active_connections: u64,
    pub pool_utilization_pct: u64,
    pub pool_over_80_since_us: u64,
}

impl Default for DbMetrics {
    fn default() -> Self {
        Self {
            pool_acquires_total: Counter::new(),
            pool_acquire_latency_us: Log2Histogram::new(),
            pool_acquire_errors_total: Counter::new(),
            pool_total_connections: GaugeU64::new(),
            pool_idle_connections: GaugeU64::new(),
            pool_active_connections: GaugeU64::new(),
            pool_pending_requests: GaugeU64::new(),
            pool_peak_active_connections: GaugeU64::new(),
            pool_over_80_since_us: GaugeU64::new(),
        }
    }
}

impl DbMetrics {
    #[must_use]
    pub fn snapshot(&self) -> DbMetricsSnapshot {
        let pool_total_connections = self.pool_total_connections.load();
        let pool_active_connections = self.pool_active_connections.load();
        let pool_utilization_pct = if pool_total_connections == 0 {
            0
        } else {
            pool_active_connections
                .saturating_mul(100)
                .saturating_div(pool_total_connections)
        };

        DbMetricsSnapshot {
            pool_acquires_total: self.pool_acquires_total.load(),
            pool_acquire_errors_total: self.pool_acquire_errors_total.load(),
            pool_acquire_latency_us: self.pool_acquire_latency_us.snapshot(),
            pool_total_connections,
            pool_idle_connections: self.pool_idle_connections.load(),
            pool_active_connections,
            pool_pending_requests: self.pool_pending_requests.load(),
            pool_peak_active_connections: self.pool_peak_active_connections.load(),
            pool_utilization_pct,
            pool_over_80_since_us: self.pool_over_80_since_us.load(),
        }
    }
}

#[derive(Debug)]
pub struct StorageMetrics {
    pub wbq_enqueued_total: Counter,
    pub wbq_drained_total: Counter,
    pub wbq_errors_total: Counter,
    pub wbq_fallbacks_total: Counter,
    pub wbq_depth: GaugeU64,
    pub wbq_capacity: GaugeU64,
    pub wbq_peak_depth: GaugeU64,
    pub wbq_over_80_since_us: GaugeU64,
    pub wbq_queue_latency_us: Log2Histogram,

    pub commit_enqueued_total: Counter,
    pub commit_drained_total: Counter,
    pub commit_errors_total: Counter,
    pub commit_sync_fallbacks_total: Counter,
    pub commit_pending_requests: GaugeU64,
    pub commit_soft_cap: GaugeU64,
    pub commit_peak_pending_requests: GaugeU64,
    pub commit_over_80_since_us: GaugeU64,
    pub commit_queue_latency_us: Log2Histogram,

    /// Count of DB rows missing corresponding archive files (set at startup).
    pub needs_reindex_total: Counter,

    // -- Git/archive IO metrics --
    /// Time spent waiting to acquire the project advisory lock (`.archive.lock`).
    pub archive_lock_wait_us: Log2Histogram,
    /// Time spent waiting for the commit/index lock in `commit_paths_with_retry`.
    pub commit_lock_wait_us: Log2Histogram,
    /// Time spent performing `commit_paths` (git index update + commit).
    pub git_commit_latency_us: Log2Histogram,
    /// Number of git index.lock retries across all `commit_paths_with_retry` calls.
    pub git_index_lock_retries_total: Counter,
    /// Number of git index.lock exhaustion failures (all retries failed).
    pub git_index_lock_failures_total: Counter,
    /// Total `commit_paths_with_retry` invocations.
    pub commit_attempts_total: Counter,
    /// Total `commit_paths_with_retry` failures (any error, not just index.lock).
    pub commit_failures_total: Counter,
    /// Number of `rel_paths` in the most recent commit call.
    pub commit_batch_size_last: GaugeU64,
    /// Successful lock-free (plumbing-based) commits that bypassed index.lock.
    pub lockfree_commits_total: Counter,
    /// Lock-free commit attempts that failed and fell back to index-based commit.
    pub lockfree_commit_fallbacks_total: Counter,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageMetricsSnapshot {
    pub wbq_enqueued_total: u64,
    pub wbq_drained_total: u64,
    pub wbq_errors_total: u64,
    pub wbq_fallbacks_total: u64,
    pub wbq_depth: u64,
    pub wbq_capacity: u64,
    pub wbq_peak_depth: u64,
    pub wbq_over_80_since_us: u64,
    pub wbq_queue_latency_us: HistogramSnapshot,

    pub commit_enqueued_total: u64,
    pub commit_drained_total: u64,
    pub commit_errors_total: u64,
    pub commit_sync_fallbacks_total: u64,
    pub commit_pending_requests: u64,
    pub commit_soft_cap: u64,
    pub commit_peak_pending_requests: u64,
    pub commit_over_80_since_us: u64,
    pub commit_queue_latency_us: HistogramSnapshot,

    pub needs_reindex_total: u64,

    pub archive_lock_wait_us: HistogramSnapshot,
    pub commit_lock_wait_us: HistogramSnapshot,
    pub git_commit_latency_us: HistogramSnapshot,
    pub git_index_lock_retries_total: u64,
    pub git_index_lock_failures_total: u64,
    pub commit_attempts_total: u64,
    pub commit_failures_total: u64,
    pub commit_batch_size_last: u64,
    pub lockfree_commits_total: u64,
    pub lockfree_commit_fallbacks_total: u64,
}

#[derive(Debug)]
pub struct SystemMetrics {
    pub disk_storage_free_bytes: GaugeU64,
    pub disk_db_free_bytes: GaugeU64,
    pub disk_effective_free_bytes: GaugeU64,
    pub disk_pressure_level: GaugeU64,
    pub disk_last_sample_us: GaugeU64,
    pub disk_sample_errors_total: Counter,

    // Memory pressure (RSS-based)
    pub memory_rss_bytes: GaugeU64,
    pub memory_pressure_level: GaugeU64,
    pub memory_last_sample_us: GaugeU64,
    pub memory_sample_errors_total: Counter,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetricsSnapshot {
    pub disk_storage_free_bytes: u64,
    pub disk_db_free_bytes: u64,
    pub disk_effective_free_bytes: u64,
    pub disk_pressure_level: u64,
    pub disk_last_sample_us: u64,
    pub disk_sample_errors_total: u64,

    pub memory_rss_bytes: u64,
    pub memory_pressure_level: u64,
    pub memory_last_sample_us: u64,
    pub memory_sample_errors_total: u64,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self {
            disk_storage_free_bytes: GaugeU64::new(),
            disk_db_free_bytes: GaugeU64::new(),
            disk_effective_free_bytes: GaugeU64::new(),
            disk_pressure_level: GaugeU64::new(),
            disk_last_sample_us: GaugeU64::new(),
            disk_sample_errors_total: Counter::new(),

            memory_rss_bytes: GaugeU64::new(),
            memory_pressure_level: GaugeU64::new(),
            memory_last_sample_us: GaugeU64::new(),
            memory_sample_errors_total: Counter::new(),
        }
    }
}

impl SystemMetrics {
    #[must_use]
    pub fn snapshot(&self) -> SystemMetricsSnapshot {
        SystemMetricsSnapshot {
            disk_storage_free_bytes: self.disk_storage_free_bytes.load(),
            disk_db_free_bytes: self.disk_db_free_bytes.load(),
            disk_effective_free_bytes: self.disk_effective_free_bytes.load(),
            disk_pressure_level: self.disk_pressure_level.load(),
            disk_last_sample_us: self.disk_last_sample_us.load(),
            disk_sample_errors_total: self.disk_sample_errors_total.load(),

            memory_rss_bytes: self.memory_rss_bytes.load(),
            memory_pressure_level: self.memory_pressure_level.load(),
            memory_last_sample_us: self.memory_last_sample_us.load(),
            memory_sample_errors_total: self.memory_sample_errors_total.load(),
        }
    }
}

impl Default for StorageMetrics {
    fn default() -> Self {
        Self {
            wbq_enqueued_total: Counter::new(),
            wbq_drained_total: Counter::new(),
            wbq_errors_total: Counter::new(),
            wbq_fallbacks_total: Counter::new(),
            wbq_depth: GaugeU64::new(),
            wbq_capacity: GaugeU64::new(),
            wbq_peak_depth: GaugeU64::new(),
            wbq_over_80_since_us: GaugeU64::new(),
            wbq_queue_latency_us: Log2Histogram::new(),

            commit_enqueued_total: Counter::new(),
            commit_drained_total: Counter::new(),
            commit_errors_total: Counter::new(),
            commit_sync_fallbacks_total: Counter::new(),
            commit_pending_requests: GaugeU64::new(),
            commit_soft_cap: GaugeU64::new(),
            commit_peak_pending_requests: GaugeU64::new(),
            commit_over_80_since_us: GaugeU64::new(),
            commit_queue_latency_us: Log2Histogram::new(),

            needs_reindex_total: Counter::new(),

            archive_lock_wait_us: Log2Histogram::new(),
            commit_lock_wait_us: Log2Histogram::new(),
            git_commit_latency_us: Log2Histogram::new(),
            git_index_lock_retries_total: Counter::new(),
            git_index_lock_failures_total: Counter::new(),
            commit_attempts_total: Counter::new(),
            commit_failures_total: Counter::new(),
            commit_batch_size_last: GaugeU64::new(),
            lockfree_commits_total: Counter::new(),
            lockfree_commit_fallbacks_total: Counter::new(),
        }
    }
}

impl StorageMetrics {
    #[must_use]
    pub fn snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot {
            wbq_enqueued_total: self.wbq_enqueued_total.load(),
            wbq_drained_total: self.wbq_drained_total.load(),
            wbq_errors_total: self.wbq_errors_total.load(),
            wbq_fallbacks_total: self.wbq_fallbacks_total.load(),
            wbq_depth: self.wbq_depth.load(),
            wbq_capacity: self.wbq_capacity.load(),
            wbq_peak_depth: self.wbq_peak_depth.load(),
            wbq_over_80_since_us: self.wbq_over_80_since_us.load(),
            wbq_queue_latency_us: self.wbq_queue_latency_us.snapshot(),

            commit_enqueued_total: self.commit_enqueued_total.load(),
            commit_drained_total: self.commit_drained_total.load(),
            commit_errors_total: self.commit_errors_total.load(),
            commit_sync_fallbacks_total: self.commit_sync_fallbacks_total.load(),
            commit_pending_requests: self.commit_pending_requests.load(),
            commit_soft_cap: self.commit_soft_cap.load(),
            commit_peak_pending_requests: self.commit_peak_pending_requests.load(),
            commit_over_80_since_us: self.commit_over_80_since_us.load(),
            commit_queue_latency_us: self.commit_queue_latency_us.snapshot(),

            needs_reindex_total: self.needs_reindex_total.load(),

            archive_lock_wait_us: self.archive_lock_wait_us.snapshot(),
            commit_lock_wait_us: self.commit_lock_wait_us.snapshot(),
            git_commit_latency_us: self.git_commit_latency_us.snapshot(),
            git_index_lock_retries_total: self.git_index_lock_retries_total.load(),
            git_index_lock_failures_total: self.git_index_lock_failures_total.load(),
            commit_attempts_total: self.commit_attempts_total.load(),
            commit_failures_total: self.commit_failures_total.load(),
            commit_batch_size_last: self.commit_batch_size_last.load(),
            lockfree_commits_total: self.lockfree_commits_total.load(),
            lockfree_commit_fallbacks_total: self.lockfree_commit_fallbacks_total.load(),
        }
    }
}

#[derive(Debug, Default)]
pub struct GlobalMetrics {
    pub http: HttpMetrics,
    pub tools: ToolsMetrics,
    pub db: DbMetrics,
    pub storage: StorageMetrics,
    pub system: SystemMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalMetricsSnapshot {
    pub http: HttpMetricsSnapshot,
    pub tools: ToolsMetricsSnapshot,
    pub db: DbMetricsSnapshot,
    pub storage: StorageMetricsSnapshot,
    pub system: SystemMetricsSnapshot,
}

impl GlobalMetrics {
    #[must_use]
    pub fn snapshot(&self) -> GlobalMetricsSnapshot {
        GlobalMetricsSnapshot {
            http: self.http.snapshot(),
            tools: self.tools.snapshot(),
            db: self.db.snapshot(),
            storage: self.storage.snapshot(),
            system: self.system.snapshot(),
        }
    }
}

static GLOBAL_METRICS: LazyLock<GlobalMetrics> = LazyLock::new(GlobalMetrics::default);

#[must_use]
pub fn global_metrics() -> &'static GlobalMetrics {
    &GLOBAL_METRICS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_bucket_indexing_smoke() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(1), 0);
        assert_eq!(bucket_index(2), 1);
        assert_eq!(bucket_index(3), 1);
        assert_eq!(bucket_index(4), 2);
        assert_eq!(bucket_index(7), 2);
        assert_eq!(bucket_index(8), 3);
    }

    #[test]
    fn histogram_snapshot_empty_is_zeros() {
        let h = Log2Histogram::new();
        let snap = h.snapshot();
        assert_eq!(snap.count, 0);
        assert_eq!(snap.min, 0);
        assert_eq!(snap.p99, 0);
    }

    #[test]
    fn histogram_quantiles_are_monotonic() {
        let h = Log2Histogram::new();
        for v in [1u64, 2, 3, 4, 10, 100, 1000, 10_000] {
            h.record(v);
        }
        let snap = h.snapshot();
        assert!(snap.p50 <= snap.p95);
        assert!(snap.p95 <= snap.p99);
        assert!(snap.max >= snap.p99);
    }

    #[test]
    fn storage_io_metrics_snapshot_includes_new_fields() {
        let m = StorageMetrics::default();

        // Simulate some IO activity.
        m.archive_lock_wait_us.record(150);
        m.commit_lock_wait_us.record(80);
        m.git_commit_latency_us.record(5_000);
        m.git_index_lock_retries_total.add(3);
        m.git_index_lock_failures_total.inc();
        m.commit_attempts_total.add(10);
        m.commit_failures_total.inc();
        m.commit_batch_size_last.set(7);
        m.lockfree_commits_total.add(5);
        m.lockfree_commit_fallbacks_total.add(2);

        let snap = m.snapshot();

        assert_eq!(snap.archive_lock_wait_us.count, 1);
        assert_eq!(snap.commit_lock_wait_us.count, 1);
        assert_eq!(snap.git_commit_latency_us.count, 1);
        assert_eq!(snap.git_index_lock_retries_total, 3);
        assert_eq!(snap.git_index_lock_failures_total, 1);
        assert_eq!(snap.commit_attempts_total, 10);
        assert_eq!(snap.commit_failures_total, 1);
        assert_eq!(snap.commit_batch_size_last, 7);
        assert_eq!(snap.lockfree_commits_total, 5);
        assert_eq!(snap.lockfree_commit_fallbacks_total, 2);

        // Verify JSON serialization includes the new keys.
        let json = serde_json::to_value(&snap).expect("snapshot should be serializable");
        assert!(json.get("archive_lock_wait_us").is_some());
        assert!(json.get("commit_lock_wait_us").is_some());
        assert!(json.get("git_commit_latency_us").is_some());
        assert!(json.get("git_index_lock_retries_total").is_some());
        assert!(json.get("git_index_lock_failures_total").is_some());
        assert!(json.get("commit_attempts_total").is_some());
        assert!(json.get("commit_failures_total").is_some());
        assert!(json.get("commit_batch_size_last").is_some());
        assert!(json.get("lockfree_commits_total").is_some());
        assert!(json.get("lockfree_commit_fallbacks_total").is_some());
    }
}
