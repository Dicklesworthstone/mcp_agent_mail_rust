//! Criterion benchmarks for the Bayesian TUI diff strategy (br-2zq8o, D.3).
//!
//! Measures per-frame decision overhead for different frame states and
//! compares Bayesian strategy throughput against a trivial always-full baseline.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use mcp_agent_mail_server::tui_decision::{BayesianDiffStrategy, FrameState};

const fn stable_frame() -> FrameState {
    FrameState {
        change_ratio: 0.05,
        is_resize: false,
        budget_remaining_ms: 14.0,
        error_count: 0,
    }
}

const fn bursty_frame() -> FrameState {
    FrameState {
        change_ratio: 0.6,
        is_resize: false,
        budget_remaining_ms: 12.0,
        error_count: 0,
    }
}

const fn resize_frame() -> FrameState {
    FrameState {
        change_ratio: 0.0,
        is_resize: true,
        budget_remaining_ms: 16.0,
        error_count: 0,
    }
}

const fn degraded_frame() -> FrameState {
    FrameState {
        change_ratio: 0.2,
        is_resize: false,
        budget_remaining_ms: 2.0,
        error_count: 5,
    }
}

/// Benchmark: 100 Bayesian strategy decisions on stable frames.
fn bench_frame_bayesian_stable(c: &mut Criterion) {
    c.bench_function("bayesian_100_stable_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for _ in 0..100 {
                let action = strategy.observe_with_ledger(&stable_frame(), None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

/// Benchmark: 100 frames with mixed conditions (cycling through all 4 states).
fn bench_frame_bayesian_mixed(c: &mut Criterion) {
    let frames = [
        stable_frame(),
        bursty_frame(),
        resize_frame(),
        degraded_frame(),
    ];
    c.bench_function("bayesian_100_mixed_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for i in 0..100 {
                let action = strategy.observe_with_ledger(&frames[i % 4], None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

/// Benchmark: baseline always-full (deterministic fallback) for comparison.
fn bench_frame_full_baseline(c: &mut Criterion) {
    c.bench_function("full_baseline_100_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            strategy.deterministic_fallback = true;
            for _ in 0..100 {
                let action = strategy.observe_with_ledger(&stable_frame(), None);
                black_box(action);
            }
        });
    });
}

/// Benchmark: 1000 frames to measure throughput at scale.
fn bench_frame_bayesian_1000(c: &mut Criterion) {
    let frames = [
        stable_frame(),
        bursty_frame(),
        resize_frame(),
        degraded_frame(),
    ];
    c.bench_function("bayesian_1000_mixed_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for i in 0..1000 {
                let action = strategy.observe_with_ledger(&frames[i % 4], None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

criterion_group!(
    benches,
    bench_frame_bayesian_stable,
    bench_frame_bayesian_mixed,
    bench_frame_full_baseline,
    bench_frame_bayesian_1000,
);

criterion_main!(benches);
