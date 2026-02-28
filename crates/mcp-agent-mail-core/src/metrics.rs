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

#[derive(Debug, Clone, Default, Serialize)]
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
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.min.fetch_min(value, Ordering::Relaxed);
        self.max.fetch_max(value, Ordering::Relaxed);
        let idx = bucket_index(value);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        // count is written LAST with Release so that an Acquire load on count
        // in snapshot() establishes a happens-before edge for all prior writes.
        self.count.fetch_add(1, Ordering::Release);
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
        // Acquire on count pairs with Release in record(), ensuring all prior
        // writes (sum, min, max, buckets) are visible.
        let count = self.count.load(Ordering::Acquire);
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

        let raw_min = self.min.load(Ordering::Relaxed);
        let max = self.max.load(Ordering::Relaxed);
        // Clamp min <= max to maintain invariant even under concurrent races.
        let min = raw_min.min(max);
        let p50 = estimate_quantile_frac(&buckets, count, 1, 2, max);
        let p95 = estimate_quantile_frac(&buckets, count, 19, 20, max);
        let p99 = estimate_quantile_frac(&buckets, count, 99, 100, max);

        HistogramSnapshot {
            count,
            sum: self.sum.load(Ordering::Relaxed),
            min,
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
    /// Total requests rejected by the rate limiter (HTTP 429).
    pub rate_limit_rejected_total: Counter,
    /// Total requests checked by the rate limiter (allowed + rejected).
    pub rate_limit_checked_total: Counter,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HttpMetricsSnapshot {
    pub requests_total: u64,
    pub requests_inflight: i64,
    pub requests_2xx: u64,
    pub requests_4xx: u64,
    pub requests_5xx: u64,
    pub latency_us: HistogramSnapshot,
    pub rate_limit_rejected_total: u64,
    pub rate_limit_checked_total: u64,
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
            rate_limit_rejected_total: Counter::new(),
            rate_limit_checked_total: Counter::new(),
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

    /// Record a rate limit check result.
    #[inline]
    pub fn record_rate_limit_check(&self, allowed: bool) {
        self.rate_limit_checked_total.inc();
        if !allowed {
            self.rate_limit_rejected_total.inc();
        }
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
            rate_limit_rejected_total: self.rate_limit_rejected_total.load(),
            rate_limit_checked_total: self.rate_limit_checked_total.load(),
        }
    }
}

#[derive(Debug)]
pub struct ToolsMetrics {
    pub tool_calls_total: Counter,
    pub tool_errors_total: Counter,
    pub tool_latency_us: Log2Histogram,
    /// Incremented when a contact enforcement DB query fails and the code
    /// falls back to empty results (fail-open). Allows alerting on silent
    /// enforcement degradation.
    pub contact_enforcement_bypass_total: Counter,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolsMetricsSnapshot {
    pub tool_calls_total: u64,
    pub tool_errors_total: u64,
    pub tool_latency_us: HistogramSnapshot,
    pub contact_enforcement_bypass_total: u64,
}

impl Default for ToolsMetrics {
    fn default() -> Self {
        Self {
            tool_calls_total: Counter::new(),
            tool_errors_total: Counter::new(),
            tool_latency_us: Log2Histogram::new(),
            contact_enforcement_bypass_total: Counter::new(),
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
            contact_enforcement_bypass_total: self.contact_enforcement_bypass_total.load(),
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

#[derive(Debug, Clone, Default, Serialize)]
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

#[derive(Debug, Clone, Default, Serialize)]
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

    // Disk I/O bytes (from /proc/self/io on Linux)
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/17
    pub disk_io_write_bytes: GaugeU64,
    pub disk_io_read_bytes: GaugeU64,

    // TUI spin watchdog (startup protection)
    pub tui_spin_watchdog_trips_total: Counter,
    pub tui_spin_watchdog_last_cpu_pct_x100: GaugeU64,
    pub tui_spin_watchdog_last_trip_us: GaugeU64,
}

#[derive(Debug, Clone, Default, Serialize)]
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

    /// Cumulative bytes written by this process (from `/proc/self/io`).
    /// Always 0 on non-Linux platforms.
    pub disk_io_write_bytes: u64,
    /// Cumulative bytes read by this process (from `/proc/self/io`).
    /// Always 0 on non-Linux platforms.
    pub disk_io_read_bytes: u64,

    pub tui_spin_watchdog_trips_total: u64,
    pub tui_spin_watchdog_last_cpu_pct_x100: u64,
    pub tui_spin_watchdog_last_trip_us: u64,
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

            disk_io_write_bytes: GaugeU64::new(),
            disk_io_read_bytes: GaugeU64::new(),

            tui_spin_watchdog_trips_total: Counter::new(),
            tui_spin_watchdog_last_cpu_pct_x100: GaugeU64::new(),
            tui_spin_watchdog_last_trip_us: GaugeU64::new(),
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

            disk_io_write_bytes: self.disk_io_write_bytes.load(),
            disk_io_read_bytes: self.disk_io_read_bytes.load(),

            tui_spin_watchdog_trips_total: self.tui_spin_watchdog_trips_total.load(),
            tui_spin_watchdog_last_cpu_pct_x100: self.tui_spin_watchdog_last_cpu_pct_x100.load(),
            tui_spin_watchdog_last_trip_us: self.tui_spin_watchdog_last_trip_us.load(),
        }
    }
}

// ---------------------------------------------------------------------------
// Search V3 Metrics
// ---------------------------------------------------------------------------

/// Search V3 telemetry and operational metrics.
///
/// Tracks query volumes, latencies, engine selection, shadow comparison results,
/// and fallback behavior for safe rollout validation.
#[derive(Debug)]
pub struct SearchMetrics {
    // -- Query volume counters --
    /// Total search queries executed (all engines).
    pub queries_total: Counter,
    /// Queries routed to V3 (Tantivy lexical or hybrid).
    pub queries_v3_total: Counter,
    /// Queries routed to legacy `SQLite` FTS5.
    pub queries_legacy_total: Counter,
    /// Shadow mode comparisons executed (both engines run).
    pub shadow_comparisons_total: Counter,
    /// Queries that encountered errors (any engine).
    pub queries_errors_total: Counter,

    // -- Latency histograms --
    /// All query latencies (microseconds).
    pub query_latency_us: Log2Histogram,
    /// V3-specific query latencies (microseconds).
    pub v3_latency_us: Log2Histogram,
    /// Legacy FTS query latencies (microseconds).
    pub legacy_latency_us: Log2Histogram,

    // -- Shadow mode metrics --
    /// Shadow comparisons where results were equivalent (≥80% overlap).
    pub shadow_equivalent_total: Counter,
    /// Shadow comparisons where V3 had errors.
    pub shadow_v3_errors_total: Counter,
    /// Shadow comparisons with significant result divergence.
    pub shadow_divergent_total: Counter,
    /// Cumulative latency delta (V3 - legacy) in shadow mode (for averaging).
    /// Stored with +1M offset to handle negative deltas in atomic u64.
    shadow_latency_delta_sum_us: AtomicU64,
    /// Count for shadow latency delta averaging.
    shadow_latency_delta_count: AtomicU64,

    // -- Fallback and degradation --
    /// V3 errors that triggered fallback to legacy FTS.
    pub fallback_to_legacy_total: Counter,
    /// Semantic tier disabled via kill switch during query.
    pub semantic_killswitch_hits: Counter,
    /// Rerank tier disabled via kill switch during query.
    pub rerank_killswitch_hits: Counter,

    // -- Index health gauges --
    /// Estimated Tantivy index size in bytes (updated periodically).
    pub tantivy_index_size_bytes: GaugeU64,
    /// Documents in Tantivy index (updated periodically).
    pub tantivy_doc_count: GaugeU64,
    /// Last index update timestamp (micros since epoch).
    pub tantivy_last_update_us: GaugeU64,
}

/// Point-in-time snapshot of search metrics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchMetricsSnapshot {
    // Query volumes
    pub queries_total: u64,
    pub queries_v3_total: u64,
    pub queries_legacy_total: u64,
    pub shadow_comparisons_total: u64,
    pub queries_errors_total: u64,

    // Latencies
    pub query_latency_us: HistogramSnapshot,
    pub v3_latency_us: HistogramSnapshot,
    pub legacy_latency_us: HistogramSnapshot,

    // Shadow mode
    pub shadow_equivalent_total: u64,
    pub shadow_equivalent_pct: f64,
    pub shadow_v3_errors_total: u64,
    pub shadow_divergent_total: u64,
    pub shadow_avg_latency_delta_us: i64,

    // Fallback/degradation
    pub fallback_to_legacy_total: u64,
    pub semantic_killswitch_hits: u64,
    pub rerank_killswitch_hits: u64,

    // Index health
    pub tantivy_index_size_bytes: u64,
    pub tantivy_doc_count: u64,
    pub tantivy_last_update_us: u64,
}

impl Default for SearchMetrics {
    fn default() -> Self {
        Self {
            queries_total: Counter::new(),
            queries_v3_total: Counter::new(),
            queries_legacy_total: Counter::new(),
            shadow_comparisons_total: Counter::new(),
            queries_errors_total: Counter::new(),

            query_latency_us: Log2Histogram::new(),
            v3_latency_us: Log2Histogram::new(),
            legacy_latency_us: Log2Histogram::new(),

            shadow_equivalent_total: Counter::new(),
            shadow_v3_errors_total: Counter::new(),
            shadow_divergent_total: Counter::new(),
            shadow_latency_delta_sum_us: AtomicU64::new(0),
            shadow_latency_delta_count: AtomicU64::new(0),

            fallback_to_legacy_total: Counter::new(),
            semantic_killswitch_hits: Counter::new(),
            rerank_killswitch_hits: Counter::new(),

            tantivy_index_size_bytes: GaugeU64::new(),
            tantivy_doc_count: GaugeU64::new(),
            tantivy_last_update_us: GaugeU64::new(),
        }
    }
}

/// Offset for storing signed latency deltas in unsigned atomics.
const LATENCY_DELTA_OFFSET: i64 = 1_000_000;

impl SearchMetrics {
    /// Record a V3 query execution.
    #[inline]
    pub fn record_v3_query(&self, latency_us: u64, is_error: bool) {
        self.queries_total.inc();
        self.queries_v3_total.inc();
        self.query_latency_us.record(latency_us);
        self.v3_latency_us.record(latency_us);
        if is_error {
            self.queries_errors_total.inc();
        }
    }

    /// Record a legacy FTS query execution.
    #[inline]
    pub fn record_legacy_query(&self, latency_us: u64, is_error: bool) {
        self.queries_total.inc();
        self.queries_legacy_total.inc();
        self.query_latency_us.record(latency_us);
        self.legacy_latency_us.record(latency_us);
        if is_error {
            self.queries_errors_total.inc();
        }
    }

    /// Record a shadow mode comparison result.
    #[allow(clippy::cast_sign_loss)]
    pub fn record_shadow_comparison(
        &self,
        is_equivalent: bool,
        v3_had_error: bool,
        latency_delta_us: i64,
    ) {
        self.shadow_comparisons_total.inc();

        if is_equivalent {
            self.shadow_equivalent_total.inc();
        } else {
            self.shadow_divergent_total.inc();
        }

        if v3_had_error {
            self.shadow_v3_errors_total.inc();
        }

        // Store latency delta with offset to handle negatives
        let delta_offset = (latency_delta_us + LATENCY_DELTA_OFFSET) as u64;
        self.shadow_latency_delta_sum_us
            .fetch_add(delta_offset, Ordering::Relaxed);
        self.shadow_latency_delta_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fallback from V3 to legacy due to error.
    #[inline]
    pub fn record_fallback(&self) {
        self.fallback_to_legacy_total.inc();
    }

    /// Record semantic kill switch activation.
    #[inline]
    pub fn record_semantic_killswitch(&self) {
        self.semantic_killswitch_hits.inc();
    }

    /// Record rerank kill switch activation.
    #[inline]
    pub fn record_rerank_killswitch(&self) {
        self.rerank_killswitch_hits.inc();
    }

    /// Update Tantivy index health gauges.
    #[allow(clippy::cast_possible_truncation)] // u128 micros won't overflow u64 for millennia
    pub fn update_index_health(&self, size_bytes: u64, doc_count: u64) {
        self.tantivy_index_size_bytes.set(size_bytes);
        self.tantivy_doc_count.set(doc_count);
        self.tantivy_last_update_us.set(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as u64),
        );
    }

    #[allow(clippy::cast_precision_loss)]
    #[allow(clippy::cast_possible_wrap)]
    #[must_use]
    pub fn snapshot(&self) -> SearchMetricsSnapshot {
        let shadow_total = self.shadow_comparisons_total.load();
        let shadow_equiv = self.shadow_equivalent_total.load();
        let shadow_equiv_pct = if shadow_total > 0 {
            shadow_equiv as f64 / shadow_total as f64 * 100.0
        } else {
            0.0
        };

        let delta_count = self.shadow_latency_delta_count.load(Ordering::Relaxed);
        let delta_sum = self.shadow_latency_delta_sum_us.load(Ordering::Relaxed);
        let avg_delta = if delta_count > 0 {
            (delta_sum as i64 / delta_count as i64) - LATENCY_DELTA_OFFSET
        } else {
            0
        };

        SearchMetricsSnapshot {
            queries_total: self.queries_total.load(),
            queries_v3_total: self.queries_v3_total.load(),
            queries_legacy_total: self.queries_legacy_total.load(),
            shadow_comparisons_total: shadow_total,
            queries_errors_total: self.queries_errors_total.load(),

            query_latency_us: self.query_latency_us.snapshot(),
            v3_latency_us: self.v3_latency_us.snapshot(),
            legacy_latency_us: self.legacy_latency_us.snapshot(),

            shadow_equivalent_total: shadow_equiv,
            shadow_equivalent_pct: shadow_equiv_pct,
            shadow_v3_errors_total: self.shadow_v3_errors_total.load(),
            shadow_divergent_total: self.shadow_divergent_total.load(),
            shadow_avg_latency_delta_us: avg_delta,

            fallback_to_legacy_total: self.fallback_to_legacy_total.load(),
            semantic_killswitch_hits: self.semantic_killswitch_hits.load(),
            rerank_killswitch_hits: self.rerank_killswitch_hits.load(),

            tantivy_index_size_bytes: self.tantivy_index_size_bytes.load(),
            tantivy_doc_count: self.tantivy_doc_count.load(),
            tantivy_last_update_us: self.tantivy_last_update_us.load(),
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
    pub search: SearchMetrics,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GlobalMetricsSnapshot {
    pub http: HttpMetricsSnapshot,
    pub tools: ToolsMetricsSnapshot,
    pub db: DbMetricsSnapshot,
    pub storage: StorageMetricsSnapshot,
    pub system: SystemMetricsSnapshot,
    pub search: SearchMetricsSnapshot,
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
            search: self.search.snapshot(),
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

    #[test]
    fn histogram_min_max_clamped_invariant() {
        use std::sync::Arc;
        use std::thread;

        let h = Arc::new(Log2Histogram::new());

        // Spawn threads to record interleaved values
        let h1 = Arc::clone(&h);
        let t1 = thread::spawn(move || {
            h1.record(1000);
        });
        let h2 = Arc::clone(&h);
        let t2 = thread::spawn(move || {
            h2.record(1);
        });
        t1.join().unwrap();
        t2.join().unwrap();

        // Snapshot must always have min <= max
        let snap = h.snapshot();
        assert!(
            snap.min <= snap.max,
            "Invariant violated: min={} > max={}",
            snap.min,
            snap.max
        );
        assert_eq!(snap.count, 2);
    }

    #[test]
    fn contact_enforcement_bypass_counter() {
        let m = ToolsMetrics::default();
        assert_eq!(m.contact_enforcement_bypass_total.load(), 0);

        m.contact_enforcement_bypass_total.inc();
        m.contact_enforcement_bypass_total.inc();
        m.contact_enforcement_bypass_total.add(3);

        let snap = m.snapshot();
        assert_eq!(snap.contact_enforcement_bypass_total, 5);

        let json = serde_json::to_value(&snap).expect("snapshot should be serializable");
        assert_eq!(json["contact_enforcement_bypass_total"], 5);
    }

    // ── br-1i11.3.6: histogram snapshot overhead benchmark ──────────────
    //
    // Quantifies the cost of Acquire/Release memory ordering on snapshot()
    // under concurrent load. Verifies that snapshot latency remains bounded
    // and that invariants hold under high contention.

    #[test]
    fn histogram_snapshot_benchmark_concurrent_recording() {
        use std::sync::Arc;
        use std::time::Instant;

        const NUM_WRITERS: usize = 8;
        const RECORDS_PER_WRITER: usize = 50_000;
        const SNAPSHOT_ITERATIONS: usize = 100;

        let h = Arc::new(Log2Histogram::new());

        // Phase 1: concurrent recording
        let write_start = Instant::now();
        std::thread::scope(|s| {
            for tid in 0..NUM_WRITERS {
                let hist = Arc::clone(&h);
                s.spawn(move || {
                    for i in 0..RECORDS_PER_WRITER {
                        hist.record((tid as u64 * 1000) + (i as u64 % 10_000));
                    }
                });
            }
        });
        let write_elapsed = write_start.elapsed();

        let total_records = (NUM_WRITERS * RECORDS_PER_WRITER) as u64;
        let snap = h.snapshot();
        assert_eq!(snap.count, total_records, "all records should be visible");

        // Phase 2: snapshot overhead benchmark
        let mut snap_times = Vec::with_capacity(SNAPSHOT_ITERATIONS);
        for _ in 0..SNAPSHOT_ITERATIONS {
            let start = Instant::now();
            let s = h.snapshot();
            #[allow(clippy::cast_precision_loss)]
            snap_times.push(start.elapsed().as_nanos() as f64);
            // Invariants must hold on every snapshot
            assert!(s.min <= s.max, "min={} > max={}", s.min, s.max);
            assert!(s.p50 <= s.p95, "p50={} > p95={}", s.p50, s.p95);
            assert!(s.p95 <= s.p99, "p95={} > p99={}", s.p95, s.p99);
        }

        #[allow(clippy::cast_precision_loss)]
        let snap_mean = snap_times.iter().sum::<f64>() / SNAPSHOT_ITERATIONS as f64;
        let snap_max = snap_times.iter().copied().fold(0.0_f64, f64::max);

        eprintln!(
            "histogram_bench writers={NUM_WRITERS} records={total_records} \
             write_ms={:.1} snap_mean_ns={snap_mean:.0} snap_max_ns={snap_max:.0} \
             iterations={SNAPSHOT_ITERATIONS}",
            write_elapsed.as_secs_f64() * 1000.0,
        );

        // Snapshot should be sub-microsecond on modern hardware
        assert!(
            snap_mean < 50_000.0,
            "snapshot mean {snap_mean:.0}ns exceeds 50µs threshold"
        );
    }

    #[test]
    fn histogram_snapshot_benchmark_concurrent_read_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        const NUM_WRITERS: usize = 4;
        const NUM_READERS: usize = 4;
        const DURATION_MS: u64 = 200;

        let h = Arc::new(Log2Histogram::new());
        let running = Arc::new(AtomicBool::new(true));
        let invariant_violations = Arc::new(std::sync::atomic::AtomicU64::new(0));

        std::thread::scope(|s| {
            // Writers: continuously record values
            for tid in 0..NUM_WRITERS {
                let hist = Arc::clone(&h);
                let run = Arc::clone(&running);
                s.spawn(move || {
                    let mut count = 0u64;
                    while run.load(AtomicOrdering::Relaxed) {
                        hist.record((tid as u64) * 100 + (count % 1000));
                        count += 1;
                    }
                    eprintln!("histogram_bench writer={tid} records={count}");
                });
            }

            // Readers: continuously take snapshots and check invariants
            for rid in 0..NUM_READERS {
                let hist = Arc::clone(&h);
                let run = Arc::clone(&running);
                let violations = Arc::clone(&invariant_violations);
                s.spawn(move || {
                    let mut snap_count = 0u64;
                    while run.load(AtomicOrdering::Relaxed) {
                        let snap = hist.snapshot();
                        if snap.count > 0 && snap.min > snap.max {
                            violations.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                        if snap.p50 > snap.p95 || snap.p95 > snap.p99 {
                            violations.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                        snap_count += 1;
                    }
                    eprintln!("histogram_bench reader={rid} snapshots={snap_count}");
                });
            }

            std::thread::sleep(std::time::Duration::from_millis(DURATION_MS));
            running.store(false, AtomicOrdering::Relaxed);
        });

        let violations = invariant_violations.load(AtomicOrdering::Relaxed);
        let final_snap = h.snapshot();
        eprintln!(
            "histogram_bench_rw total_records={} violations={violations}",
            final_snap.count
        );
        assert_eq!(
            violations, 0,
            "snapshot invariants violated {violations} times under concurrent read/write"
        );
    }

    #[test]
    fn histogram_snapshot_quantile_stability_under_load() {
        use std::sync::Arc;

        let h = Arc::new(Log2Histogram::new());

        // Record a known bimodal distribution across threads
        std::thread::scope(|s| {
            // Low-latency cluster: 10-100µs
            for _ in 0..4 {
                let hist = Arc::clone(&h);
                s.spawn(move || {
                    for v in 10..=100 {
                        for _ in 0..100 {
                            hist.record(v);
                        }
                    }
                });
            }
            // High-latency cluster: 10000-50000µs
            for _ in 0..2 {
                let hist = Arc::clone(&h);
                s.spawn(move || {
                    for v in (10_000..=50_000).step_by(100) {
                        for _ in 0..10 {
                            hist.record(v);
                        }
                    }
                });
            }
        });

        let snap = h.snapshot();
        eprintln!(
            "histogram_quantile_stability count={} min={} max={} p50={} p95={} p99={}",
            snap.count, snap.min, snap.max, snap.p50, snap.p95, snap.p99
        );

        assert!(snap.min <= snap.max);
        assert!(snap.p50 <= snap.p95);
        assert!(snap.p95 <= snap.p99);
        // p50 should be in the low-latency cluster (most records are there)
        assert!(
            snap.p50 <= 200,
            "p50={} should be in low-latency cluster (≤200)",
            snap.p50
        );
        // p99 should be in the high-latency cluster
        assert!(
            snap.p99 >= 1000,
            "p99={} should reflect high-latency tail (≥1000)",
            snap.p99
        );
    }

    // ── Search metrics tests ─────────────────────────────────────────────────

    #[test]
    fn search_metrics_record_v3_query() {
        let m = SearchMetrics::default();

        m.record_v3_query(1000, false);
        m.record_v3_query(2000, false);
        m.record_v3_query(5000, true); // error

        let snap = m.snapshot();
        assert_eq!(snap.queries_total, 3);
        assert_eq!(snap.queries_v3_total, 3);
        assert_eq!(snap.queries_legacy_total, 0);
        assert_eq!(snap.queries_errors_total, 1);
        assert_eq!(snap.v3_latency_us.count, 3);
    }

    #[test]
    fn search_metrics_record_legacy_query() {
        let m = SearchMetrics::default();

        m.record_legacy_query(500, false);
        m.record_legacy_query(1500, false);

        let snap = m.snapshot();
        assert_eq!(snap.queries_total, 2);
        assert_eq!(snap.queries_v3_total, 0);
        assert_eq!(snap.queries_legacy_total, 2);
        assert_eq!(snap.legacy_latency_us.count, 2);
    }

    #[test]
    fn search_metrics_shadow_comparison() {
        let m = SearchMetrics::default();

        // Equivalent result, V3 faster by 100µs
        m.record_shadow_comparison(true, false, -100);
        // Divergent result, V3 slower by 500µs
        m.record_shadow_comparison(false, false, 500);
        // V3 error
        m.record_shadow_comparison(false, true, 1000);

        let snap = m.snapshot();
        assert_eq!(snap.shadow_comparisons_total, 3);
        assert_eq!(snap.shadow_equivalent_total, 1);
        assert_eq!(snap.shadow_divergent_total, 2);
        assert_eq!(snap.shadow_v3_errors_total, 1);
        // Average delta: (-100 + 500 + 1000) / 3 = 466
        assert!(
            (snap.shadow_avg_latency_delta_us - 466).abs() <= 1,
            "avg_delta={} expected ~466",
            snap.shadow_avg_latency_delta_us
        );
        // Equivalent percentage: 1/3 * 100 = 33.33...
        assert!(
            (snap.shadow_equivalent_pct - 33.33).abs() < 1.0,
            "equiv_pct={} expected ~33.33",
            snap.shadow_equivalent_pct
        );
    }

    #[test]
    fn search_metrics_fallback_and_killswitch() {
        let m = SearchMetrics::default();

        m.record_fallback();
        m.record_fallback();
        m.record_semantic_killswitch();
        m.record_rerank_killswitch();
        m.record_rerank_killswitch();
        m.record_rerank_killswitch();

        let snap = m.snapshot();
        assert_eq!(snap.fallback_to_legacy_total, 2);
        assert_eq!(snap.semantic_killswitch_hits, 1);
        assert_eq!(snap.rerank_killswitch_hits, 3);
    }

    #[test]
    fn search_metrics_index_health() {
        let m = SearchMetrics::default();

        m.update_index_health(1024 * 1024 * 50, 10_000);

        let snap = m.snapshot();
        assert_eq!(snap.tantivy_index_size_bytes, 50 * 1024 * 1024);
        assert_eq!(snap.tantivy_doc_count, 10_000);
        assert!(snap.tantivy_last_update_us > 0);
    }

    #[test]
    fn search_metrics_snapshot_serialization() {
        let m = SearchMetrics::default();
        m.record_v3_query(1000, false);
        m.record_shadow_comparison(true, false, 50);

        let snap = m.snapshot();
        let json = serde_json::to_value(&snap).expect("should serialize");

        // Verify key fields present
        assert!(json.get("queries_total").is_some());
        assert!(json.get("queries_v3_total").is_some());
        assert!(json.get("shadow_comparisons_total").is_some());
        assert!(json.get("shadow_equivalent_pct").is_some());
        assert!(json.get("shadow_avg_latency_delta_us").is_some());
        assert!(json.get("v3_latency_us").is_some());
        assert!(json.get("tantivy_index_size_bytes").is_some());
    }

    #[test]
    fn global_metrics_includes_search() {
        let gm = GlobalMetrics::default();
        gm.search.record_v3_query(500, false);

        let snap = gm.snapshot();
        assert_eq!(snap.search.queries_total, 1);
        assert_eq!(snap.search.queries_v3_total, 1);

        // Verify JSON includes search section
        let json = serde_json::to_value(&snap).expect("should serialize");
        assert!(json.get("search").is_some());
    }

    // ── Counter primitive ─────────────────────────────────────────────

    #[test]
    fn counter_store_and_load() {
        let c = Counter::new();
        assert_eq!(c.load(), 0);
        c.store(42);
        assert_eq!(c.load(), 42);
        c.store(0);
        assert_eq!(c.load(), 0);
    }

    #[test]
    fn counter_inc_and_add() {
        let c = Counter::new();
        c.inc();
        c.inc();
        assert_eq!(c.load(), 2);
        c.add(10);
        assert_eq!(c.load(), 12);
    }

    // ── GaugeI64 primitive ────────────────────────────────────────────

    #[test]
    fn gauge_i64_add_set_load() {
        let g = GaugeI64::new();
        assert_eq!(g.load(), 0);
        g.set(100);
        assert_eq!(g.load(), 100);
        g.add(-30);
        assert_eq!(g.load(), 70);
        g.add(5);
        assert_eq!(g.load(), 75);
    }

    #[test]
    fn gauge_i64_negative_values() {
        let g = GaugeI64::new();
        g.set(-50);
        assert_eq!(g.load(), -50);
        g.add(-10);
        assert_eq!(g.load(), -60);
    }

    // ── GaugeU64 primitive ────────────────────────────────────────────

    #[test]
    fn gauge_u64_add() {
        let g = GaugeU64::new();
        g.add(5);
        g.add(3);
        assert_eq!(g.load(), 8);
    }

    #[test]
    fn gauge_u64_fetch_max() {
        let g = GaugeU64::new();
        g.set(10);
        g.fetch_max(5); // 5 < 10, no change
        assert_eq!(g.load(), 10);
        g.fetch_max(20); // 20 > 10, update
        assert_eq!(g.load(), 20);
        g.fetch_max(20); // equal, no change
        assert_eq!(g.load(), 20);
    }

    // ── Log2Histogram reset ───────────────────────────────────────────

    #[test]
    fn histogram_reset_clears_all_state() {
        let h = Log2Histogram::new();
        h.record(100);
        h.record(200);
        h.record(300);
        let snap = h.snapshot();
        assert_eq!(snap.count, 3);

        h.reset();
        let snap2 = h.snapshot();
        assert_eq!(snap2.count, 0);
        assert_eq!(snap2.sum, 0);
        assert_eq!(snap2.min, 0);
        assert_eq!(snap2.max, 0);
    }

    // ── HttpMetrics record + snapshot ─────────────────────────────────

    #[test]
    fn http_metrics_record_response_and_snapshot() {
        let m = HttpMetrics::default();
        m.record_response(200, 1000);
        m.record_response(201, 2000);
        m.record_response(404, 500);
        m.record_response(500, 3000);
        m.record_response(302, 100); // not 2xx/4xx/5xx

        let snap = m.snapshot();
        assert_eq!(snap.requests_total, 5);
        assert_eq!(snap.requests_2xx, 2);
        assert_eq!(snap.requests_4xx, 1);
        assert_eq!(snap.requests_5xx, 1);
        assert_eq!(snap.latency_us.count, 5);
    }

    #[test]
    fn http_metrics_inflight_tracking() {
        let m = HttpMetrics::default();
        m.requests_inflight.add(1);
        m.requests_inflight.add(1);
        m.requests_inflight.add(-1);
        let snap = m.snapshot();
        assert_eq!(snap.requests_inflight, 1);
    }

    // ── DbMetrics snapshot with utilization ───────────────────────────

    #[test]
    fn db_metrics_snapshot_utilization_pct() {
        let m = DbMetrics::default();
        m.pool_total_connections.set(100);
        m.pool_active_connections.set(75);
        let snap = m.snapshot();
        assert_eq!(snap.pool_utilization_pct, 75);
    }

    #[test]
    fn db_metrics_snapshot_utilization_zero_total() {
        let m = DbMetrics::default();
        // total_connections = 0 → no division by zero
        let snap = m.snapshot();
        assert_eq!(snap.pool_utilization_pct, 0);
    }

    // ── SystemMetrics snapshot ────────────────────────────────────────

    #[test]
    fn system_metrics_snapshot_round_trip() {
        let m = SystemMetrics::default();
        m.disk_storage_free_bytes.set(1_000_000);
        m.disk_pressure_level.set(2);
        let snap = m.snapshot();
        assert_eq!(snap.disk_storage_free_bytes, 1_000_000);
        assert_eq!(snap.disk_pressure_level, 2);
    }

    // ── global_metrics() singleton ────────────────────────────────────

    #[test]
    fn global_metrics_returns_consistent_reference() {
        let gm1 = super::global_metrics();
        let gm2 = super::global_metrics();
        assert!(std::ptr::eq(gm1, gm2));
    }
}
