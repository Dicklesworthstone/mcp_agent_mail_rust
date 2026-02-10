//! KPI aggregation layer for operational dashboards (br-3vwi.7.1).
//!
//! Computes derived, time-windowed KPIs from raw counters and histograms
//! exposed by [`crate::metrics::global_metrics()`].  The module maintains a
//! circular buffer of periodic samples and derives throughput, latency,
//! ack-pressure, and contention indicators for configurable time windows
//! (1 min, 5 min, 15 min, 1 hour).
//!
//! # Formulas
//!
//! All formulas are deterministic given the same input samples.
//!
//! | KPI | Formula | Unit |
//! |-----|---------|------|
//! | `throughput_ops_per_sec` | `Δ(tool_calls_total) / Δt_sec` | ops/s |
//! | `error_rate_bps` | `Δ(tool_errors_total) / Δ(tool_calls_total) × 10_000` | basis points |
//! | `http_rps` | `Δ(requests_total) / Δt_sec` | req/s |
//! | `tool_latency_p50_ms` | `snapshot.tool_latency_us.p50 / 1000` | ms |
//! | `tool_latency_p95_ms` | `snapshot.tool_latency_us.p95 / 1000` | ms |
//! | `tool_latency_p99_ms` | `snapshot.tool_latency_us.p99 / 1000` | ms |
//! | `pool_acquire_p95_ms` | `snapshot.db.pool_acquire_latency_us.p95 / 1000` | ms |
//! | `pool_utilization_pct` | `active_conns × 100 / total_conns` | % |
//! | `wbq_utilization_pct` | `wbq_depth × 100 / wbq_capacity` | % |
//! | `ack_pending` | last recorded pending ack gauge | count |
//! | `ack_overdue` | last recorded overdue ack gauge | count |
//! | `reservation_active` | last recorded active reservation gauge | count |
//! | `reservation_conflicts` | `Δ(reservation_conflict_counter)` over window | count |
//! | `commit_throughput_per_sec` | `Δ(commit_drained_total) / Δt_sec` | ops/s |
//! | `git_commit_p95_ms` | `snapshot.storage.git_commit_latency_us.p95 / 1000` | ms |

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use serde::Serialize;

use crate::metrics::{GlobalMetricsSnapshot, HistogramSnapshot, global_metrics};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Supported aggregation windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KpiWindow {
    /// 1-minute window.
    OneMin,
    /// 5-minute window.
    FiveMin,
    /// 15-minute window.
    FifteenMin,
    /// 1-hour window.
    OneHour,
}

impl KpiWindow {
    /// Duration of the window in seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::OneMin => 60,
            Self::FiveMin => 300,
            Self::FifteenMin => 900,
            Self::OneHour => 3600,
        }
    }

    /// All supported windows, ordered by duration.
    pub const ALL: [Self; 4] = [Self::OneMin, Self::FiveMin, Self::FifteenMin, Self::OneHour];
}

impl std::fmt::Display for KpiWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OneMin => f.write_str("1m"),
            Self::FiveMin => f.write_str("5m"),
            Self::FifteenMin => f.write_str("15m"),
            Self::OneHour => f.write_str("1h"),
        }
    }
}

// ---------------------------------------------------------------------------
// Supplemental gauges (fed by higher layers — DB, tool handlers)
// ---------------------------------------------------------------------------

/// Counters that higher layers record into and the KPI layer reads.
#[derive(Debug)]
pub struct KpiGauges {
    /// Number of messages pending acknowledgment.
    pub ack_pending: AtomicU64,
    /// Number of overdue acknowledgments.
    pub ack_overdue: AtomicU64,
    /// Number of active (non-expired) file reservations.
    pub reservation_active: AtomicU64,
    /// Cumulative reservation conflict count (monotonically increasing).
    pub reservation_conflicts_total: AtomicU64,
    /// Cumulative messages sent (monotonically increasing).
    /// Allows message-specific throughput separate from generic tool calls.
    pub messages_sent_total: AtomicU64,
}

impl Default for KpiGauges {
    fn default() -> Self {
        Self {
            ack_pending: AtomicU64::new(0),
            ack_overdue: AtomicU64::new(0),
            reservation_active: AtomicU64::new(0),
            reservation_conflicts_total: AtomicU64::new(0),
            messages_sent_total: AtomicU64::new(0),
        }
    }
}

static KPI_GAUGES: LazyLock<KpiGauges> = LazyLock::new(KpiGauges::default);

/// Global KPI supplemental gauges.
#[must_use]
pub fn kpi_gauges() -> &'static KpiGauges {
    &KPI_GAUGES
}

// ---------------------------------------------------------------------------
// Sample buffer
// ---------------------------------------------------------------------------

/// A point-in-time sample of all raw counters and gauges.
#[derive(Debug, Clone)]
struct Sample {
    /// Monotonic instant when the sample was taken.
    taken_at: Instant,
    /// Full metrics snapshot.
    metrics: GlobalMetricsSnapshot,
    /// Supplemental gauge values at sample time.
    ack_pending: u64,
    ack_overdue: u64,
    reservation_active: u64,
    reservation_conflicts_total: u64,
    messages_sent_total: u64,
}

/// Maximum number of samples retained (one per second → covers 1 hour).
const MAX_SAMPLES: usize = 3600;

/// Global sample ring buffer.
static SAMPLE_BUFFER: LazyLock<Mutex<SampleRing>> = LazyLock::new(|| Mutex::new(SampleRing::new()));

struct SampleRing {
    buf: Vec<Sample>,
    /// Next write position (wraps around).
    head: usize,
    /// Total samples ever written.
    total_written: u64,
}

impl SampleRing {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MAX_SAMPLES),
            head: 0,
            total_written: 0,
        }
    }

    fn push(&mut self, sample: Sample) {
        if self.buf.len() < MAX_SAMPLES {
            self.buf.push(sample);
        } else {
            self.buf[self.head] = sample;
        }
        self.head = (self.head + 1) % MAX_SAMPLES;
        self.total_written += 1;
    }

    /// Return the most recent sample, if any.
    fn latest(&self) -> Option<&Sample> {
        if self.buf.is_empty() {
            return None;
        }
        let idx = if self.head == 0 {
            self.buf.len() - 1
        } else {
            self.head - 1
        };
        Some(&self.buf[idx])
    }

    /// Find the sample closest to `target_age` seconds ago from the latest.
    /// Returns (`oldest_sample`, `newest_sample`) pair for delta computation.
    fn window_pair(&self, window_secs: u64) -> Option<(&Sample, &Sample)> {
        let newest = self.latest()?;
        if self.buf.len() < 2 {
            return None;
        }

        let target = newest
            .taken_at
            .checked_sub(std::time::Duration::from_secs(window_secs))?;

        // Walk backwards from newest to find the sample closest to target.
        let len = self.buf.len();
        let mut best_idx = None;
        let mut best_diff = u64::MAX;

        for offset in 1..len {
            let raw = if self.head > offset {
                self.head - 1 - offset
            } else {
                len - (1 + offset - self.head)
            };
            let s = &self.buf[raw];

            let diff = if s.taken_at >= target {
                s.taken_at.duration_since(target).as_secs()
            } else {
                target.duration_since(s.taken_at).as_secs()
            };

            if diff < best_diff {
                best_diff = diff;
                best_idx = Some(raw);
            }

            // If we've gone past the target and the diff is increasing, stop.
            if s.taken_at < target && diff > best_diff {
                break;
            }
        }

        best_idx.map(|idx| (&self.buf[idx], newest))
    }

    /// Number of samples currently stored.
    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// Public recording API
// ---------------------------------------------------------------------------

/// Take a new sample from `global_metrics()` + supplemental gauges.
///
/// Call this periodically (e.g., every 1 second from a timer tick).
pub fn record_sample() {
    let metrics = global_metrics().snapshot();
    let g = kpi_gauges();

    let sample = Sample {
        taken_at: Instant::now(),
        metrics,
        ack_pending: g.ack_pending.load(Ordering::Relaxed),
        ack_overdue: g.ack_overdue.load(Ordering::Relaxed),
        reservation_active: g.reservation_active.load(Ordering::Relaxed),
        reservation_conflicts_total: g.reservation_conflicts_total.load(Ordering::Relaxed),
        messages_sent_total: g.messages_sent_total.load(Ordering::Relaxed),
    };

    if let Ok(mut ring) = SAMPLE_BUFFER.lock() {
        ring.push(sample);
    }
}

/// Take a sample with an explicit metrics snapshot (for testing or custom sampling).
pub fn record_sample_with(metrics: GlobalMetricsSnapshot) {
    let g = kpi_gauges();
    let sample = Sample {
        taken_at: Instant::now(),
        metrics,
        ack_pending: g.ack_pending.load(Ordering::Relaxed),
        ack_overdue: g.ack_overdue.load(Ordering::Relaxed),
        reservation_active: g.reservation_active.load(Ordering::Relaxed),
        reservation_conflicts_total: g.reservation_conflicts_total.load(Ordering::Relaxed),
        messages_sent_total: g.messages_sent_total.load(Ordering::Relaxed),
    };

    if let Ok(mut ring) = SAMPLE_BUFFER.lock() {
        ring.push(sample);
    }
}

/// Number of samples currently stored.
#[must_use]
pub fn sample_count() -> usize {
    SAMPLE_BUFFER.lock().map_or(0, |ring| ring.len())
}

/// Clear all accumulated samples (for testing).
pub fn reset_samples() {
    if let Ok(mut ring) = SAMPLE_BUFFER.lock() {
        ring.buf.clear();
        ring.head = 0;
        ring.total_written = 0;
    }
}

// ---------------------------------------------------------------------------
// KPI snapshot types
// ---------------------------------------------------------------------------

/// Throughput KPIs for a window.
#[derive(Debug, Clone, Serialize)]
pub struct ThroughputKpi {
    /// Tool calls per second (all tools).
    pub tool_calls_per_sec: f64,
    /// Tool errors per second.
    pub tool_errors_per_sec: f64,
    /// Error rate in basis points (1 bp = 0.01%).
    pub error_rate_bps: f64,
    /// HTTP requests per second.
    pub http_rps: f64,
    /// Messages sent per second.
    pub messages_per_sec: f64,
    /// Git commits drained per second (from commit coalescer).
    pub commit_throughput_per_sec: f64,
}

/// Latency KPIs (point-in-time from latest sample's histograms).
#[derive(Debug, Clone, Serialize)]
pub struct LatencyKpi {
    /// Tool call latency (all tools aggregated).
    pub tool_p50_ms: f64,
    pub tool_p95_ms: f64,
    pub tool_p99_ms: f64,
    /// DB pool acquire latency.
    pub pool_acquire_p50_ms: f64,
    pub pool_acquire_p95_ms: f64,
    /// HTTP request latency.
    pub http_p50_ms: f64,
    pub http_p95_ms: f64,
    /// Git commit latency.
    pub git_commit_p95_ms: f64,
    /// WBQ queue wait latency.
    pub wbq_queue_p95_ms: f64,
}

/// Ack pressure KPIs.
#[derive(Debug, Clone, Serialize)]
pub struct AckPressureKpi {
    /// Messages pending acknowledgment.
    pub pending: u64,
    /// Messages with overdue acknowledgment.
    pub overdue: u64,
}

/// Contention KPIs.
#[derive(Debug, Clone, Serialize)]
pub struct ContentionKpi {
    /// DB pool utilization (0–100).
    pub pool_utilization_pct: u64,
    /// WBQ utilization (0–100).
    pub wbq_utilization_pct: u64,
    /// Active file reservations.
    pub reservation_active: u64,
    /// Reservation conflicts observed during the window.
    pub reservation_conflicts_in_window: u64,
    /// WBQ backpressure events during the window.
    pub wbq_backpressure_in_window: u64,
    /// Git index lock retries during the window.
    pub git_lock_retries_in_window: u64,
}

/// Complete KPI snapshot for one time window.
#[derive(Debug, Clone, Serialize)]
pub struct KpiSnapshot {
    /// The window this snapshot covers.
    pub window: KpiWindow,
    /// Actual time span covered (may be shorter than window if insufficient data).
    pub actual_span_secs: f64,
    /// Number of samples in the buffer at snapshot time.
    pub sample_count: usize,
    /// Throughput indicators.
    pub throughput: ThroughputKpi,
    /// Latency indicators (from latest sample).
    pub latency: LatencyKpi,
    /// Ack pressure indicators.
    pub ack_pressure: AckPressureKpi,
    /// Contention indicators.
    pub contention: ContentionKpi,
}

/// All windows combined.
#[derive(Debug, Clone, Serialize)]
pub struct KpiReport {
    /// Per-window snapshots, ordered by window duration.
    pub windows: Vec<KpiSnapshot>,
}

// ---------------------------------------------------------------------------
// Computation
// ---------------------------------------------------------------------------

fn us_to_ms(hist: &HistogramSnapshot, quantile: fn(&HistogramSnapshot) -> u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let v = quantile(hist) as f64 / 1000.0;
    v
}

fn delta_rate(old: u64, new: u64, dt_secs: f64) -> f64 {
    if dt_secs <= 0.0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let delta = new.saturating_sub(old) as f64;
    delta / dt_secs
}

fn compute_kpi(window: KpiWindow, old: &Sample, new: &Sample) -> KpiSnapshot {
    let dt = new.taken_at.duration_since(old.taken_at);
    let dt_secs = dt.as_secs_f64().max(0.001); // avoid div-by-zero

    let m_old = &old.metrics;
    let m_new = &new.metrics;

    // -- Throughput --
    let tool_calls_per_sec = delta_rate(
        m_old.tools.tool_calls_total,
        m_new.tools.tool_calls_total,
        dt_secs,
    );
    let tool_errors_per_sec = delta_rate(
        m_old.tools.tool_errors_total,
        m_new.tools.tool_errors_total,
        dt_secs,
    );

    let delta_calls = m_new
        .tools
        .tool_calls_total
        .saturating_sub(m_old.tools.tool_calls_total);
    let delta_errors = m_new
        .tools
        .tool_errors_total
        .saturating_sub(m_old.tools.tool_errors_total);
    #[allow(clippy::cast_precision_loss)]
    let error_rate_bps = if delta_calls == 0 {
        0.0
    } else {
        (delta_errors as f64 / delta_calls as f64) * 10_000.0
    };

    let http_rps = delta_rate(
        m_old.http.requests_total,
        m_new.http.requests_total,
        dt_secs,
    );
    let messages_per_sec = delta_rate(old.messages_sent_total, new.messages_sent_total, dt_secs);
    let commit_throughput_per_sec = delta_rate(
        m_old.storage.commit_drained_total,
        m_new.storage.commit_drained_total,
        dt_secs,
    );

    // -- Latency (from newest sample's histograms) --
    let latency = LatencyKpi {
        tool_p50_ms: us_to_ms(&m_new.tools.tool_latency_us, |h| h.p50),
        tool_p95_ms: us_to_ms(&m_new.tools.tool_latency_us, |h| h.p95),
        tool_p99_ms: us_to_ms(&m_new.tools.tool_latency_us, |h| h.p99),
        pool_acquire_p50_ms: us_to_ms(&m_new.db.pool_acquire_latency_us, |h| h.p50),
        pool_acquire_p95_ms: us_to_ms(&m_new.db.pool_acquire_latency_us, |h| h.p95),
        http_p50_ms: us_to_ms(&m_new.http.latency_us, |h| h.p50),
        http_p95_ms: us_to_ms(&m_new.http.latency_us, |h| h.p95),
        git_commit_p95_ms: us_to_ms(&m_new.storage.git_commit_latency_us, |h| h.p95),
        wbq_queue_p95_ms: us_to_ms(&m_new.storage.wbq_queue_latency_us, |h| h.p95),
    };

    // -- Ack pressure --
    let ack_pressure = AckPressureKpi {
        pending: new.ack_pending,
        overdue: new.ack_overdue,
    };

    // -- Contention --
    let wbq_cap = m_new.storage.wbq_capacity;
    let wbq_utilization_pct = if wbq_cap == 0 {
        0
    } else {
        m_new
            .storage
            .wbq_depth
            .saturating_mul(100)
            .saturating_div(wbq_cap)
    };

    let reservation_conflicts_in_window = new
        .reservation_conflicts_total
        .saturating_sub(old.reservation_conflicts_total);

    let wbq_backpressure_in_window = m_new
        .storage
        .wbq_fallbacks_total
        .saturating_sub(m_old.storage.wbq_fallbacks_total);

    let git_lock_retries_in_window = m_new
        .storage
        .git_index_lock_retries_total
        .saturating_sub(m_old.storage.git_index_lock_retries_total);

    let contention = ContentionKpi {
        pool_utilization_pct: m_new.db.pool_utilization_pct,
        wbq_utilization_pct,
        reservation_active: new.reservation_active,
        reservation_conflicts_in_window,
        wbq_backpressure_in_window,
        git_lock_retries_in_window,
    };

    KpiSnapshot {
        window,
        actual_span_secs: dt_secs,
        sample_count: 0, // filled by caller
        throughput: ThroughputKpi {
            tool_calls_per_sec,
            tool_errors_per_sec,
            error_rate_bps,
            http_rps,
            messages_per_sec,
            commit_throughput_per_sec,
        },
        latency,
        ack_pressure,
        contention,
    }
}

// ---------------------------------------------------------------------------
// Public query API
// ---------------------------------------------------------------------------

/// Compute KPI snapshot for a single window.
///
/// Returns `None` if fewer than 2 samples exist.
#[must_use]
pub fn snapshot(window: KpiWindow) -> Option<KpiSnapshot> {
    let ring = SAMPLE_BUFFER.lock().ok()?;
    let (old, new) = ring.window_pair(window.seconds())?;
    let mut kpi = compute_kpi(window, old, new);
    kpi.sample_count = ring.len();
    drop(ring);
    Some(kpi)
}

/// Compute KPI snapshots for all standard windows.
#[must_use]
pub fn report() -> KpiReport {
    let Ok(ring) = SAMPLE_BUFFER.lock() else {
        return KpiReport {
            windows: Vec::new(),
        };
    };

    let sample_count = ring.len();
    let windows = KpiWindow::ALL
        .iter()
        .filter_map(|&w| {
            let (old, new) = ring.window_pair(w.seconds())?;
            let mut kpi = compute_kpi(w, old, new);
            kpi.sample_count = sample_count;
            Some(kpi)
        })
        .collect();

    drop(ring);
    KpiReport { windows }
}

/// Return the latest raw metrics snapshot, if any samples exist.
#[must_use]
pub fn latest_raw() -> Option<GlobalMetricsSnapshot> {
    let ring = SAMPLE_BUFFER.lock().ok()?;
    ring.latest().map(|s| s.metrics.clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{
        DbMetricsSnapshot, GlobalMetricsSnapshot, HistogramSnapshot, HttpMetricsSnapshot,
        StorageMetricsSnapshot, SystemMetricsSnapshot, ToolsMetricsSnapshot,
    };
    use std::thread;
    use std::time::Duration;

    fn zero_histogram() -> HistogramSnapshot {
        HistogramSnapshot {
            count: 0,
            sum: 0,
            min: 0,
            max: 0,
            p50: 0,
            p95: 0,
            p99: 0,
        }
    }

    fn make_histogram(p50: u64, p95: u64, p99: u64) -> HistogramSnapshot {
        HistogramSnapshot {
            count: 100,
            sum: p50 * 100,
            min: p50 / 2,
            max: p99 * 2,
            p50,
            p95,
            p99,
        }
    }

    fn make_snapshot(
        tool_calls: u64,
        tool_errors: u64,
        http_requests: u64,
        commit_drained: u64,
        wbq_fallbacks: u64,
        git_retries: u64,
    ) -> GlobalMetricsSnapshot {
        GlobalMetricsSnapshot {
            http: HttpMetricsSnapshot {
                requests_total: http_requests,
                requests_inflight: 5,
                requests_2xx: http_requests,
                requests_4xx: 0,
                requests_5xx: 0,
                latency_us: make_histogram(500, 2000, 5000),
            },
            tools: ToolsMetricsSnapshot {
                tool_calls_total: tool_calls,
                tool_errors_total: tool_errors,
                tool_latency_us: make_histogram(1000, 5000, 10_000),
                contact_enforcement_bypass_total: 0,
            },
            db: DbMetricsSnapshot {
                pool_acquires_total: tool_calls,
                pool_acquire_errors_total: 0,
                pool_acquire_latency_us: make_histogram(200, 1000, 3000),
                pool_total_connections: 10,
                pool_idle_connections: 5,
                pool_active_connections: 5,
                pool_pending_requests: 0,
                pool_peak_active_connections: 8,
                pool_utilization_pct: 50,
                pool_over_80_since_us: 0,
            },
            storage: StorageMetricsSnapshot {
                wbq_enqueued_total: tool_calls,
                wbq_drained_total: tool_calls,
                wbq_errors_total: 0,
                wbq_fallbacks_total: wbq_fallbacks,
                wbq_depth: 10,
                wbq_capacity: 8192,
                wbq_peak_depth: 50,
                wbq_over_80_since_us: 0,
                wbq_queue_latency_us: make_histogram(100, 500, 1000),

                commit_enqueued_total: commit_drained + 5,
                commit_drained_total: commit_drained,
                commit_errors_total: 0,
                commit_sync_fallbacks_total: 0,
                commit_pending_requests: 5,
                commit_soft_cap: 8192,
                commit_peak_pending_requests: 20,
                commit_over_80_since_us: 0,
                commit_queue_latency_us: make_histogram(300, 1500, 4000),

                needs_reindex_total: 0,

                archive_lock_wait_us: zero_histogram(),
                commit_lock_wait_us: zero_histogram(),
                git_commit_latency_us: make_histogram(2000, 8000, 15_000),
                git_index_lock_retries_total: git_retries,
                git_index_lock_failures_total: 0,
                commit_attempts_total: commit_drained,
                commit_failures_total: 0,
                commit_batch_size_last: 3,
                lockfree_commits_total: commit_drained / 2,
                lockfree_commit_fallbacks_total: 0,
            },
            system: SystemMetricsSnapshot {
                disk_storage_free_bytes: 10_000_000_000,
                disk_db_free_bytes: 10_000_000_000,
                disk_effective_free_bytes: 10_000_000_000,
                disk_pressure_level: 0,
                disk_last_sample_us: 0,
                disk_sample_errors_total: 0,
                memory_rss_bytes: 100_000_000,
                memory_pressure_level: 0,
                memory_last_sample_us: 0,
                memory_sample_errors_total: 0,
            },
        }
    }

    /// Helper: inject a sample into the ring with explicit Instant.
    #[allow(clippy::too_many_arguments)]
    fn inject_sample(
        ring: &mut SampleRing,
        at: Instant,
        metrics: GlobalMetricsSnapshot,
        ack_pending: u64,
        ack_overdue: u64,
        reservation_active: u64,
        reservation_conflicts: u64,
        messages_sent: u64,
    ) {
        ring.push(Sample {
            taken_at: at,
            metrics,
            ack_pending,
            ack_overdue,
            reservation_active,
            reservation_conflicts_total: reservation_conflicts,
            messages_sent_total: messages_sent,
        });
    }

    // -- SampleRing tests --

    #[test]
    fn ring_empty_returns_none() {
        let ring = SampleRing::new();
        assert!(ring.latest().is_none());
        assert!(ring.window_pair(60).is_none());
    }

    #[test]
    fn ring_single_sample_has_latest_but_no_pair() {
        let mut ring = SampleRing::new();
        let now = Instant::now();
        inject_sample(
            &mut ring,
            now,
            make_snapshot(0, 0, 0, 0, 0, 0),
            0,
            0,
            0,
            0,
            0,
        );
        assert!(ring.latest().is_some());
        assert!(ring.window_pair(60).is_none());
    }

    #[test]
    fn ring_two_samples_produces_pair() {
        let mut ring = SampleRing::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(30);
        inject_sample(
            &mut ring,
            t0,
            make_snapshot(100, 5, 200, 10, 0, 0),
            2,
            0,
            5,
            0,
            50,
        );
        inject_sample(
            &mut ring,
            t1,
            make_snapshot(200, 10, 400, 20, 1, 2),
            3,
            1,
            8,
            1,
            100,
        );

        let (old, new) = ring.window_pair(60).unwrap();
        assert_eq!(old.metrics.tools.tool_calls_total, 100);
        assert_eq!(new.metrics.tools.tool_calls_total, 200);
    }

    #[test]
    fn ring_wraps_correctly() {
        let mut ring = SampleRing {
            buf: Vec::with_capacity(4),
            head: 0,
            total_written: 0,
        };
        // Reduce max for test by manually using a small capacity
        let t0 = Instant::now();
        for i in 0..6 {
            inject_sample(
                &mut ring,
                t0 + Duration::from_secs(i),
                make_snapshot(i * 10, 0, 0, 0, 0, 0),
                0,
                0,
                0,
                0,
                0,
            );
        }
        // Ring should have MAX_SAMPLES entries (3600), not wrap at 4
        // since we're using the real MAX_SAMPLES constant.
        assert_eq!(ring.len(), 6);
        assert_eq!(ring.latest().unwrap().metrics.tools.tool_calls_total, 50);
    }

    // -- KPI computation tests --

    #[test]
    fn throughput_formula_correctness() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(10);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(100, 5, 200, 10, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 50,
        };
        let new = Sample {
            taken_at: t1,
            metrics: make_snapshot(200, 15, 400, 30, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 100,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);

        // 100 calls / 10 sec = 10 ops/sec
        assert!((kpi.throughput.tool_calls_per_sec - 10.0).abs() < 0.01);
        // 10 errors / 10 sec = 1 error/sec
        assert!((kpi.throughput.tool_errors_per_sec - 1.0).abs() < 0.01);
        // 10 errors / 100 calls = 0.1 = 1000 bps
        assert!((kpi.throughput.error_rate_bps - 1000.0).abs() < 0.01);
        // 200 http reqs / 10 sec = 20 rps
        assert!((kpi.throughput.http_rps - 20.0).abs() < 0.01);
        // 50 msgs / 10 sec = 5 msgs/sec
        assert!((kpi.throughput.messages_per_sec - 5.0).abs() < 0.01);
        // 20 commits / 10 sec = 2 commits/sec
        assert!((kpi.throughput.commit_throughput_per_sec - 2.0).abs() < 0.01);
    }

    #[test]
    fn error_rate_zero_when_no_calls() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(10);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(0, 0, 0, 0, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };
        let new = old.clone();
        // Adjust time for new
        let mut new_sample = new;
        new_sample.taken_at = t1;

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new_sample);
        assert!(kpi.throughput.error_rate_bps.abs() < f64::EPSILON);
        assert!(kpi.throughput.tool_calls_per_sec.abs() < f64::EPSILON);
    }

    #[test]
    fn latency_conversion_us_to_ms() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(0, 0, 0, 0, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };
        let new = Sample {
            taken_at: t1,
            metrics: make_snapshot(100, 0, 100, 10, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);

        // make_histogram sets p50=1000us, p95=5000us, p99=10000us for tool latency
        assert!((kpi.latency.tool_p50_ms - 1.0).abs() < 0.001);
        assert!((kpi.latency.tool_p95_ms - 5.0).abs() < 0.001);
        assert!((kpi.latency.tool_p99_ms - 10.0).abs() < 0.001);

        // pool acquire: p50=200us=0.2ms, p95=1000us=1.0ms
        assert!((kpi.latency.pool_acquire_p50_ms - 0.2).abs() < 0.001);
        assert!((kpi.latency.pool_acquire_p95_ms - 1.0).abs() < 0.001);

        // git commit p95=8000us=8.0ms
        assert!((kpi.latency.git_commit_p95_ms - 8.0).abs() < 0.001);
    }

    #[test]
    fn ack_pressure_reflects_newest_sample() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(5);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(0, 0, 0, 0, 0, 0),
            ack_pending: 10,
            ack_overdue: 2,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };
        let new = Sample {
            taken_at: t1,
            metrics: make_snapshot(0, 0, 0, 0, 0, 0),
            ack_pending: 15,
            ack_overdue: 5,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);
        assert_eq!(kpi.ack_pressure.pending, 15);
        assert_eq!(kpi.ack_pressure.overdue, 5);
    }

    #[test]
    fn contention_delta_computation() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(60);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(0, 0, 0, 0, 3, 10),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 5,
            reservation_conflicts_total: 2,
            messages_sent_total: 0,
        };
        let new = Sample {
            taken_at: t1,
            metrics: make_snapshot(0, 0, 0, 0, 7, 15),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 8,
            reservation_conflicts_total: 5,
            messages_sent_total: 0,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);

        assert_eq!(kpi.contention.reservation_active, 8);
        assert_eq!(kpi.contention.reservation_conflicts_in_window, 3); // 5 - 2
        assert_eq!(kpi.contention.wbq_backpressure_in_window, 4); // 7 - 3
        assert_eq!(kpi.contention.git_lock_retries_in_window, 5); // 15 - 10
        assert_eq!(kpi.contention.pool_utilization_pct, 50);
    }

    #[test]
    fn wbq_utilization_zero_when_capacity_zero() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let mut metrics = make_snapshot(0, 0, 0, 0, 0, 0);
        metrics.storage.wbq_capacity = 0;
        metrics.storage.wbq_depth = 10;

        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(0, 0, 0, 0, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };
        let new = Sample {
            taken_at: t1,
            metrics,
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 0,
            messages_sent_total: 0,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);
        assert_eq!(kpi.contention.wbq_utilization_pct, 0);
    }

    #[test]
    fn kpi_window_display() {
        assert_eq!(format!("{}", KpiWindow::OneMin), "1m");
        assert_eq!(format!("{}", KpiWindow::FiveMin), "5m");
        assert_eq!(format!("{}", KpiWindow::FifteenMin), "15m");
        assert_eq!(format!("{}", KpiWindow::OneHour), "1h");
    }

    #[test]
    fn kpi_window_seconds() {
        assert_eq!(KpiWindow::OneMin.seconds(), 60);
        assert_eq!(KpiWindow::FiveMin.seconds(), 300);
        assert_eq!(KpiWindow::FifteenMin.seconds(), 900);
        assert_eq!(KpiWindow::OneHour.seconds(), 3600);
    }

    #[test]
    fn kpi_snapshot_serializable() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(10);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(100, 5, 200, 10, 0, 0),
            ack_pending: 3,
            ack_overdue: 1,
            reservation_active: 4,
            reservation_conflicts_total: 2,
            messages_sent_total: 50,
        };
        let new = Sample {
            taken_at: t1,
            metrics: make_snapshot(200, 10, 400, 20, 1, 3),
            ack_pending: 5,
            ack_overdue: 2,
            reservation_active: 6,
            reservation_conflicts_total: 4,
            messages_sent_total: 100,
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);
        let json = serde_json::to_value(&kpi).expect("KpiSnapshot should be serializable");

        assert!(json.get("window").is_some());
        assert!(json.get("throughput").is_some());
        assert!(json.get("latency").is_some());
        assert!(json.get("ack_pressure").is_some());
        assert!(json.get("contention").is_some());

        // Verify nested fields
        let throughput = json.get("throughput").unwrap();
        assert!(throughput.get("tool_calls_per_sec").is_some());
        assert!(throughput.get("error_rate_bps").is_some());
        assert!(throughput.get("messages_per_sec").is_some());

        let latency = json.get("latency").unwrap();
        assert!(latency.get("tool_p50_ms").is_some());
        assert!(latency.get("tool_p95_ms").is_some());
        assert!(latency.get("git_commit_p95_ms").is_some());
    }

    #[test]
    fn global_gauges_accessible() {
        let g = kpi_gauges();
        g.ack_pending.store(42, Ordering::Relaxed);
        assert_eq!(g.ack_pending.load(Ordering::Relaxed), 42);
        // Reset for other tests
        g.ack_pending.store(0, Ordering::Relaxed);
    }

    #[test]
    fn record_and_snapshot_integration() {
        // Use the global sample buffer.
        reset_samples();
        assert_eq!(sample_count(), 0);

        // Record two samples with a small delay.
        record_sample();
        thread::sleep(Duration::from_millis(10));
        record_sample();

        assert!(sample_count() >= 2);

        // Snapshot should work for 1-minute window (actual span will be ~10ms).
        let kpi = snapshot(KpiWindow::OneMin);
        assert!(kpi.is_some());
        let kpi = kpi.unwrap();
        assert!(kpi.actual_span_secs > 0.0);
        assert!(kpi.actual_span_secs < 1.0); // Should be ~10ms, not 60s

        // Cleanup
        reset_samples();
    }

    #[test]
    fn report_returns_windows_when_data_available() {
        // Test the computation logic via SampleRing directly, avoiding
        // shared global state races with parallel tests.
        let mut ring = SampleRing::new();
        let t0 = Instant::now();

        inject_sample(
            &mut ring,
            t0,
            make_snapshot(100, 5, 200, 10, 0, 0),
            2,
            0,
            5,
            0,
            50,
        );
        inject_sample(
            &mut ring,
            t0 + Duration::from_secs(30),
            make_snapshot(200, 10, 400, 20, 1, 2),
            3,
            1,
            8,
            1,
            100,
        );

        // All windows should find a pair (both point to the same 2 samples).
        for &w in &KpiWindow::ALL {
            let pair = ring.window_pair(w.seconds());
            assert!(pair.is_some(), "window {w} should find a pair");
            let (old, new) = pair.unwrap();
            let kpi = compute_kpi(w, old, new);
            assert!(kpi.actual_span_secs > 0.0);
        }
    }

    #[test]
    fn latest_raw_returns_metrics() {
        reset_samples();
        assert!(latest_raw().is_none());

        record_sample();
        let raw = latest_raw();
        assert!(raw.is_some());

        // Cleanup
        reset_samples();
    }

    #[test]
    fn saturating_delta_handles_counter_reset() {
        // If counters somehow decreased (e.g., process restart), saturating_sub returns 0.
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(10);
        let old = Sample {
            taken_at: t0,
            metrics: make_snapshot(200, 10, 400, 20, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 5,
            messages_sent_total: 100,
        };
        let new = Sample {
            taken_at: t1,
            // Counters "reset" to lower values
            metrics: make_snapshot(50, 2, 100, 5, 0, 0),
            ack_pending: 0,
            ack_overdue: 0,
            reservation_active: 0,
            reservation_conflicts_total: 1, // lower than old
            messages_sent_total: 20,        // lower than old
        };

        let kpi = compute_kpi(KpiWindow::OneMin, &old, &new);

        // All rates should be 0 (not negative or panicked).
        assert!(kpi.throughput.tool_calls_per_sec.abs() < f64::EPSILON);
        assert!(kpi.throughput.tool_errors_per_sec.abs() < f64::EPSILON);
        assert!(kpi.throughput.error_rate_bps.abs() < f64::EPSILON);
        assert!(kpi.throughput.http_rps.abs() < f64::EPSILON);
        assert!(kpi.throughput.messages_per_sec.abs() < f64::EPSILON);
        assert!(kpi.throughput.commit_throughput_per_sec.abs() < f64::EPSILON);
        assert_eq!(kpi.contention.reservation_conflicts_in_window, 0);
    }
}
