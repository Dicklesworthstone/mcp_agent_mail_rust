//! Criterion benchmarks for `HeatmapGrid` layout caching (br-3m7xo, F.2).
//!
//! Measures frame times with and without layout cache reuse to validate
//! that caching improves performance for stable frames while not regressing
//! changing-data frames.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ftui::GraphemePool;
use ftui::layout::Rect;
use ftui::render::frame::Frame;
use ftui::widgets::Widget;
use mcp_agent_mail_server::tui_widgets::HeatmapGrid;

/// Build a 20x20 data grid with deterministic values.
fn make_grid_20x20() -> Vec<Vec<f64>> {
    (0..20)
        .map(|r| (0..20).map(|c| f64::from(r * 20 + c) / 400.0).collect())
        .collect()
}

/// Build a 5x5 data grid.
fn make_grid_5x5() -> Vec<Vec<f64>> {
    (0..5)
        .map(|r| (0..5).map(|c| f64::from(r * 5 + c) / 24.0).collect())
        .collect()
}

/// Benchmark: 100 stable frames with cache (data and area unchanged).
fn bench_heatmap_stable_100_frames(c: &mut Criterion) {
    let data = make_grid_20x20();
    let area = Rect::new(0, 0, 80, 24);

    c.bench_function("heatmap_stable_100_frames_cached", |b| {
        b.iter(|| {
            let widget = HeatmapGrid::new(&data).data_generation(0);
            let mut pool = GraphemePool::new();
            for _ in 0..100 {
                let mut frame = Frame::new(80, 24, &mut pool);
                widget.render(area, &mut frame);
            }
            black_box(widget.layout_cache().compute_count)
        });
    });
}

/// Benchmark: 100 frames where data changes every frame (cache invalidated).
fn bench_heatmap_changing_100_frames(c: &mut Criterion) {
    let data = make_grid_20x20();
    let area = Rect::new(0, 0, 80, 24);

    c.bench_function("heatmap_changing_100_frames", |b| {
        b.iter(|| {
            let mut pool = GraphemePool::new();
            for generation in 0..100u64 {
                let widget = HeatmapGrid::new(&data).data_generation(generation);
                let mut frame = Frame::new(80, 24, &mut pool);
                widget.render(area, &mut frame);
            }
        });
    });
}

/// Benchmark: 100 stable frames explicitly invalidating cache every frame.
fn bench_heatmap_uncached_100_frames(c: &mut Criterion) {
    let data = make_grid_20x20();
    let area = Rect::new(0, 0, 80, 24);

    c.bench_function("heatmap_stable_100_frames_uncached", |b| {
        b.iter(|| {
            let widget = HeatmapGrid::new(&data).data_generation(0);
            let mut pool = GraphemePool::new();
            for _ in 0..100 {
                widget.invalidate_cache();
                let mut frame = Frame::new(80, 24, &mut pool);
                widget.render(area, &mut frame);
            }
            black_box(widget.layout_cache().compute_count)
        });
    });
}

/// Benchmark: focus ring rendering 100 stable frames.
fn bench_focus_ring_stable_100_frames(c: &mut Criterion) {
    let area = Rect::new(0, 0, 40, 12);

    c.bench_function("focus_ring_stable_100_frames", |b| {
        b.iter(|| {
            let a11y = mcp_agent_mail_server::tui_widgets::A11yConfig::default();
            let mut cache = mcp_agent_mail_server::tui_widgets::FocusRingCache::new();
            let mut pool = GraphemePool::new();
            for _ in 0..100 {
                let mut frame = Frame::new(40, 12, &mut pool);
                mcp_agent_mail_server::tui_widgets::render_focus_ring_cached(
                    area,
                    &mut frame,
                    &a11y,
                    Some(&mut cache),
                );
            }
            black_box(cache.compute_count)
        });
    });
}

/// Benchmark: 5x5 golden snapshot (small grid for comparison baseline).
fn bench_heatmap_5x5_golden(c: &mut Criterion) {
    let data = make_grid_5x5();
    let row_labels: &[&str] = &["A", "B", "C", "D", "E"];
    let col_labels: &[&str] = &["1", "2", "3", "4", "5"];
    let area = Rect::new(0, 0, 30, 8);

    c.bench_function("heatmap_5x5_golden", |b| {
        b.iter(|| {
            let widget = HeatmapGrid::new(&data)
                .row_labels(row_labels)
                .col_labels(col_labels)
                .data_generation(0);
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(30, 8, &mut pool);
            widget.render(area, &mut frame);
            black_box(widget.layout_cache().compute_count)
        });
    });
}

criterion_group!(
    widget_benches,
    bench_heatmap_stable_100_frames,
    bench_heatmap_changing_100_frames,
    bench_heatmap_uncached_100_frames,
    bench_focus_ring_stable_100_frames,
    bench_heatmap_5x5_golden,
);
criterion_main!(widget_benches);
