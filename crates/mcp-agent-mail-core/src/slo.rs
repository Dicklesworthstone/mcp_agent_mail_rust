//! Service Level Objectives (SLOs) for MCP Agent Mail at 1000+ concurrent agents.
//!
//! These constants define the performance contract enforced by benchmarks
//! and stress tests (br-15dv.8.*, br-15dv.9.*). They are the single source
//! of truth for latency budgets, throughput targets, and resource limits.
//!
//! # Workload Model
//!
//! The reference scenario assumes:
//! - 50 projects, 20 agents per project (1000 agents total)
//! - 200 concurrent in-flight requests (sustained)
//! - Request mix: 40% `fetch_inbox`/resources, 25% search/summarize,
//!   15% `send_message`, 10% reservations/ack, 10% identity/macros
//! - Burst patterns: message storms (50 agents send within 1s),
//!   polling storms (200 agents poll inbox simultaneously)
//! - Body sizes: median 200 bytes, p95 2 KB, max 64 KB
//! - Attachment sizes: median 8 KB, p95 128 KB, max 5 MB (WebP converted)

// ── Workload model constants ────────────────────────────────────────

/// Target number of concurrent agents for the reference workload.
pub const WORKLOAD_AGENTS: u32 = 1000;

/// Target number of projects in the reference workload.
pub const WORKLOAD_PROJECTS: u32 = 50;

/// Agents per project in the reference workload.
pub const WORKLOAD_AGENTS_PER_PROJECT: u32 = 20;

/// Sustained concurrent in-flight requests.
pub const WORKLOAD_CONCURRENCY: u32 = 200;

// ── Tool call latency SLOs (microseconds) ───────────────────────────

/// Generic tool call p95 latency budget.
pub const TOOL_P95_US: u64 = 100_000; // 100 ms

/// Generic tool call p99 latency budget.
pub const TOOL_P99_US: u64 = 250_000; // 250 ms

/// Read-only calls (`fetch_inbox`, resources, search) p95.
pub const READ_P95_US: u64 = 50_000; // 50 ms

/// Read-only calls p99.
pub const READ_P99_US: u64 = 150_000; // 150 ms

/// `send_message` (no attachments) p95.
pub const SEND_P95_US: u64 = 150_000; // 150 ms

/// `send_message` (no attachments) p99.
pub const SEND_P99_US: u64 = 400_000; // 400 ms

/// `send_message` (with attachments) p95.
pub const SEND_ATTACH_P95_US: u64 = 500_000; // 500 ms

/// `send_message` (with attachments) p99.
pub const SEND_ATTACH_P99_US: u64 = 1_500_000; // 1500 ms

// ── DB pool acquire latency thresholds (microseconds) ───────────────

/// Pool acquire latency: Green zone (healthy).
pub const POOL_ACQUIRE_GREEN_US: u64 = 10_000; // 10 ms

/// Pool acquire latency: Yellow zone (warning).
pub const POOL_ACQUIRE_YELLOW_US: u64 = 50_000; // 50 ms

/// Pool acquire latency: Red zone (critical).
pub const POOL_ACQUIRE_RED_US: u64 = 200_000; // 200 ms

// ── Queue and resource budgets ──────────────────────────────────────

/// Maximum acceptable Write-Behind Queue (WBQ) depth before backpressure.
pub const WBQ_MAX_DEPTH: u32 = 500;

/// Maximum acceptable commit backlog (pending git commits).
pub const COMMIT_BACKLOG_MAX: u32 = 100;

/// Target WBQ drain rate (items per second).
pub const WBQ_DRAIN_RATE_PER_SEC: u32 = 50;

// ── Error rate targets ──────────────────────────────────────────────

/// Maximum acceptable error rate under normal load (basis points, 10 = 0.1%).
pub const ERROR_RATE_MAX_BP: u32 = 10;

// Policy: overload responses must be 429/503, never 500.

// ── Memory and stability ────────────────────────────────────────────

/// Maximum acceptable RSS growth per hour under sustained load (bytes).
/// Zero unbounded growth; this budget allows for cache warming only.
pub const RSS_GROWTH_MAX_BYTES_PER_HOUR: u64 = 50 * 1024 * 1024; // 50 MB

// ── Helpers ─────────────────────────────────────────────────────────

/// Health classification for pool acquire latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolHealth {
    /// Latency <= `POOL_ACQUIRE_GREEN_US`.
    Green,
    /// Latency <= `POOL_ACQUIRE_YELLOW_US`.
    Yellow,
    /// Latency > `POOL_ACQUIRE_YELLOW_US`.
    Red,
}

impl PoolHealth {
    /// Classify a pool acquire latency in microseconds.
    #[must_use]
    pub const fn classify(latency_us: u64) -> Self {
        if latency_us <= POOL_ACQUIRE_GREEN_US {
            Self::Green
        } else if latency_us <= POOL_ACQUIRE_YELLOW_US {
            Self::Yellow
        } else {
            Self::Red
        }
    }
}

impl std::fmt::Display for PoolHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => f.write_str("Green"),
            Self::Yellow => f.write_str("Yellow"),
            Self::Red => f.write_str("Red"),
        }
    }
}

/// Operation class for SLO lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// Generic tool call.
    Tool,
    /// Read-only operations (`fetch_inbox`, resources, search).
    Read,
    /// `send_message` without attachments.
    Send,
    /// `send_message` with attachments.
    SendAttach,
}

impl OpClass {
    /// Return (p95, p99) latency budgets in microseconds for this operation class.
    #[must_use]
    pub const fn budget_us(self) -> (u64, u64) {
        match self {
            Self::Tool => (TOOL_P95_US, TOOL_P99_US),
            Self::Read => (READ_P95_US, READ_P99_US),
            Self::Send => (SEND_P95_US, SEND_P99_US),
            Self::SendAttach => (SEND_ATTACH_P95_US, SEND_ATTACH_P99_US),
        }
    }
}

impl std::fmt::Display for OpClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool => f.write_str("tool"),
            Self::Read => f.write_str("read"),
            Self::Send => f.write_str("send"),
            Self::SendAttach => f.write_str("send+attach"),
        }
    }
}

// ── Compile-time invariants ─────────────────────────────────────────
// These are checked at compile time; a violation is a build error.

const _: () = {
    assert!(READ_P95_US < READ_P99_US);
    assert!(TOOL_P95_US < TOOL_P99_US);
    assert!(SEND_P95_US < SEND_P99_US);
    assert!(SEND_ATTACH_P95_US < SEND_ATTACH_P99_US);
    assert!(READ_P95_US < TOOL_P95_US);
    assert!(READ_P99_US < TOOL_P99_US);
    assert!(SEND_P95_US < SEND_ATTACH_P95_US);
    assert!(SEND_P99_US < SEND_ATTACH_P99_US);
    assert!(POOL_ACQUIRE_GREEN_US < POOL_ACQUIRE_YELLOW_US);
    assert!(POOL_ACQUIRE_YELLOW_US < POOL_ACQUIRE_RED_US);
    assert!(WORKLOAD_AGENTS == WORKLOAD_PROJECTS * WORKLOAD_AGENTS_PER_PROJECT);
    assert!(ERROR_RATE_MAX_BP <= 100);
    assert!(WBQ_MAX_DEPTH / WBQ_DRAIN_RATE_PER_SEC <= 30);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_health_classify() {
        assert_eq!(PoolHealth::classify(0), PoolHealth::Green);
        assert_eq!(
            PoolHealth::classify(POOL_ACQUIRE_GREEN_US),
            PoolHealth::Green
        );
        assert_eq!(
            PoolHealth::classify(POOL_ACQUIRE_GREEN_US + 1),
            PoolHealth::Yellow
        );
        assert_eq!(
            PoolHealth::classify(POOL_ACQUIRE_YELLOW_US),
            PoolHealth::Yellow
        );
        assert_eq!(
            PoolHealth::classify(POOL_ACQUIRE_YELLOW_US + 1),
            PoolHealth::Red
        );
        assert_eq!(PoolHealth::classify(POOL_ACQUIRE_RED_US), PoolHealth::Red);
    }

    #[test]
    fn pool_health_display() {
        assert_eq!(format!("{}", PoolHealth::Green), "Green");
        assert_eq!(format!("{}", PoolHealth::Yellow), "Yellow");
        assert_eq!(format!("{}", PoolHealth::Red), "Red");
    }

    #[test]
    fn op_class_display() {
        assert_eq!(format!("{}", OpClass::Tool), "tool");
        assert_eq!(format!("{}", OpClass::Read), "read");
        assert_eq!(format!("{}", OpClass::Send), "send");
        assert_eq!(format!("{}", OpClass::SendAttach), "send+attach");
    }

    #[test]
    fn op_class_budget_returns_correct_values() {
        assert_eq!(OpClass::Tool.budget_us(), (TOOL_P95_US, TOOL_P99_US));
        assert_eq!(OpClass::Read.budget_us(), (READ_P95_US, READ_P99_US));
        assert_eq!(OpClass::Send.budget_us(), (SEND_P95_US, SEND_P99_US));
        assert_eq!(
            OpClass::SendAttach.budget_us(),
            (SEND_ATTACH_P95_US, SEND_ATTACH_P99_US)
        );
    }

    #[test]
    fn pool_health_large_values_stay_red() {
        assert_eq!(
            PoolHealth::classify(POOL_ACQUIRE_YELLOW_US + 10_000),
            PoolHealth::Red
        );
        assert_eq!(PoolHealth::classify(u64::MAX), PoolHealth::Red);
    }

    // ── br-3h13: Additional slo.rs test coverage ──────────────────

    #[test]
    fn slo_constants_have_expected_values() {
        // Verify key constants haven't drifted from spec
        assert_eq!(TOOL_P95_US, 100_000);
        assert_eq!(TOOL_P99_US, 250_000);
        assert_eq!(READ_P95_US, 50_000);
        assert_eq!(READ_P99_US, 150_000);
        assert_eq!(SEND_P95_US, 150_000);
        assert_eq!(SEND_P99_US, 400_000);
        assert_eq!(SEND_ATTACH_P95_US, 500_000);
        assert_eq!(SEND_ATTACH_P99_US, 1_500_000);
    }

    #[test]
    fn workload_model_constants() {
        assert_eq!(WORKLOAD_AGENTS, 1000);
        assert_eq!(WORKLOAD_PROJECTS, 50);
        assert_eq!(WORKLOAD_AGENTS_PER_PROJECT, 20);
        assert_eq!(WORKLOAD_CONCURRENCY, 200);
    }

    #[test]
    fn pool_acquire_constants() {
        assert_eq!(POOL_ACQUIRE_GREEN_US, 10_000);
        assert_eq!(POOL_ACQUIRE_YELLOW_US, 50_000);
        assert_eq!(POOL_ACQUIRE_RED_US, 200_000);
    }

    #[test]
    fn queue_and_error_constants() {
        assert_eq!(WBQ_MAX_DEPTH, 500);
        assert_eq!(COMMIT_BACKLOG_MAX, 100);
        assert_eq!(WBQ_DRAIN_RATE_PER_SEC, 50);
        assert_eq!(ERROR_RATE_MAX_BP, 10);
        assert_eq!(RSS_GROWTH_MAX_BYTES_PER_HOUR, 50 * 1024 * 1024);
    }

    #[test]
    fn pool_health_boundary_values() {
        // Exact boundaries between zones
        assert_eq!(PoolHealth::classify(10_000), PoolHealth::Green);
        assert_eq!(PoolHealth::classify(10_001), PoolHealth::Yellow);
        assert_eq!(PoolHealth::classify(50_000), PoolHealth::Yellow);
        assert_eq!(PoolHealth::classify(50_001), PoolHealth::Red);
    }

    #[test]
    fn pool_health_zero_is_green() {
        assert_eq!(PoolHealth::classify(0), PoolHealth::Green);
    }

    #[test]
    fn op_class_debug_format() {
        // Ensure Debug is implemented and produces reasonable output
        let debug = format!("{:?}", OpClass::Tool);
        assert!(debug.contains("Tool"));
        let debug = format!("{:?}", OpClass::SendAttach);
        assert!(debug.contains("SendAttach"));
    }

    #[test]
    fn pool_health_debug_format() {
        let debug = format!("{:?}", PoolHealth::Green);
        assert!(debug.contains("Green"));
        let debug = format!("{:?}", PoolHealth::Red);
        assert!(debug.contains("Red"));
    }

    #[test]
    fn op_class_budget_ordering_invariants() {
        let ordered = [
            (OpClass::Read, "read"),
            (OpClass::Tool, "tool"),
            (OpClass::Send, "send"),
            (OpClass::SendAttach, "send+attach"),
        ];

        let mut prev_p95 = 0;
        let mut prev_p99 = 0;

        for (class, label) in ordered {
            let (p95, p99) = class.budget_us();
            assert!(
                p95 < p99,
                "{label} budget should have p95 < p99, got p95={p95} p99={p99}"
            );
            assert!(
                p95 > prev_p95,
                "{label} p95 should be strictly increasing across operation classes"
            );
            assert!(
                p99 > prev_p99,
                "{label} p99 should be strictly increasing across operation classes"
            );
            prev_p95 = p95;
            prev_p99 = p99;
        }
    }
}
