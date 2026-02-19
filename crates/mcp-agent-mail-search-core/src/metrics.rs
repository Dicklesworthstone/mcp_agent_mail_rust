//! Two-tier search observability metrics and alerting.
//!
//! This module tracks:
//! - initialization timing and availability
//! - per-query phase latency and refinement behavior
//! - index coverage and memory footprint
//! - rolling latency percentiles for operational dashboards
//! - threshold-based warning alerts

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auto_init::TwoTierAvailability;
use std::hash::{Hash, Hasher};

/// Required tracing span names for two-tier observability.
pub const REQUIRED_TWO_TIER_SPANS: [&str; 8] = [
    "two_tier.init",
    "two_tier.search",
    "two_tier.embed_fast",
    "two_tier.search_fast",
    "two_tier.embed_quality",
    "two_tier.score_quality",
    "two_tier.blend",
    "two_tier.rerank",
];

const DEFAULT_LATENCY_WINDOW_SIZE: usize = 512;
const COUNTER_WARN_THRESHOLD: u64 = u64::MAX - (u64::MAX / 10);

/// Initialization metrics for two-tier search startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TwoTierInitMetrics {
    /// Unix timestamp of initialization.
    pub init_timestamp: i64,
    /// Duration of fast embedder load (ms).
    pub fast_embedder_load_ms: u64,
    /// Duration of quality embedder load (ms).
    pub quality_embedder_load_ms: u64,
    /// Availability status after initialization.
    pub availability: TwoTierAvailability,
    /// Number of init attempts.
    pub init_attempts: u32,
}

/// Per-query timing and behavior metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TwoTierSearchMetrics {
    /// Query text hash for correlation.
    pub query_hash: u64,
    /// Fast embedding latency (µs).
    pub fast_embed_us: u64,
    /// Fast search latency (µs).
    pub fast_search_us: u64,
    /// Quality embedding latency (µs).
    pub quality_embed_us: u64,
    /// Quality scoring latency (µs).
    pub quality_score_us: u64,
    /// Score blending latency (µs).
    pub blend_us: u64,
    /// Number of candidates from fast search.
    pub fast_candidate_count: usize,
    /// Number of candidates refined.
    pub refined_count: usize,
    /// Whether quality refinement was executed.
    pub was_refined: bool,
    /// Whether refinement changed ranking order.
    pub ranking_changed: bool,
}

impl TwoTierSearchMetrics {
    /// Construct per-query metrics with deterministic hash.
    #[must_use]
    pub fn for_query(query: &str) -> Self {
        Self {
            query_hash: {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                query.hash(&mut h);
                h.finish()
            },
            ..Self::default()
        }
    }

    /// Combined fast-tier latency (µs).
    #[must_use]
    pub const fn fast_latency_us(&self) -> u64 {
        self.fast_embed_us.saturating_add(self.fast_search_us)
    }

    /// Combined quality refinement latency (µs).
    #[must_use]
    pub const fn quality_latency_us(&self) -> u64 {
        self.quality_embed_us
            .saturating_add(self.quality_score_us)
            .saturating_add(self.blend_us)
    }

    /// Combined end-to-end measured latency (µs).
    #[must_use]
    pub const fn total_latency_us(&self) -> u64 {
        self.fast_latency_us()
            .saturating_add(self.quality_latency_us())
    }
}

/// Index shape, coverage, and memory metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TwoTierIndexMetrics {
    /// Total documents in index.
    pub doc_count: usize,
    /// Documents with quality embeddings.
    pub quality_doc_count: usize,
    /// Quality coverage ratio (0.0-1.0).
    pub quality_coverage: f32,
    /// Fast embedding memory usage (bytes).
    pub fast_memory_bytes: usize,
    /// Quality embedding memory usage (bytes).
    pub quality_memory_bytes: usize,
    /// Total memory usage (bytes).
    pub total_memory_bytes: usize,
}

impl TwoTierIndexMetrics {
    /// Build metrics from primitive index counts.
    #[must_use]
    pub fn from_counts(
        doc_count: usize,
        quality_doc_count: usize,
        fast_memory_bytes: usize,
        quality_memory_bytes: usize,
    ) -> Self {
        let quality_coverage = if doc_count == 0 {
            1.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                quality_doc_count as f32 / doc_count as f32
            }
        };
        Self {
            doc_count,
            quality_doc_count,
            quality_coverage,
            fast_memory_bytes,
            quality_memory_bytes,
            total_memory_bytes: fast_memory_bytes.saturating_add(quality_memory_bytes),
        }
    }
}

/// Rolling aggregate metrics for dashboards and status endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TwoTierAggregatedMetrics {
    /// Total searches observed.
    pub total_searches: u64,
    /// Searches where quality refinement was attempted.
    pub refined_searches: u64,
    /// Searches where refinement changed ranking.
    pub ranking_changed_count: u64,
    /// P50 fast-tier latency (µs).
    pub fast_latency_p50_us: u64,
    /// P95 fast-tier latency (µs).
    pub fast_latency_p95_us: u64,
    /// P50 total latency (µs).
    pub total_latency_p50_us: u64,
    /// P95 total latency (µs).
    pub total_latency_p95_us: u64,
}

/// Configurable two-tier alert thresholds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TwoTierAlertConfig {
    /// Warn if fast search exceeds this (µs).
    pub fast_latency_warn_us: u64,
    /// Warn if quality refinement exceeds this (µs).
    pub quality_latency_warn_us: u64,
    /// Warn if quality coverage drops below this (percent).
    pub quality_coverage_warn_pct: f32,
    /// Warn if index exceeds this many docs.
    pub index_size_warn_docs: usize,
    /// Warn if total memory usage exceeds this (bytes).
    pub memory_warn_bytes: usize,
}

impl Default for TwoTierAlertConfig {
    fn default() -> Self {
        Self {
            fast_latency_warn_us: 5_000,
            quality_latency_warn_us: 300_000,
            quality_coverage_warn_pct: 50.0,
            index_size_warn_docs: 80_000,
            memory_warn_bytes: 500 * 1024 * 1024,
        }
    }
}

/// Alert evaluation result for the latest snapshot.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TwoTierAlertState {
    pub slow_fast_search: bool,
    pub slow_quality_refinement: bool,
    pub low_quality_coverage: bool,
    pub large_index: bool,
    pub high_memory: bool,
}

/// Read-only snapshot of all tracked two-tier metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TwoTierMetricsSnapshot {
    pub init: Option<TwoTierInitMetrics>,
    pub search: Option<TwoTierSearchMetrics>,
    pub index: TwoTierIndexMetrics,
    pub aggregated: TwoTierAggregatedMetrics,
}

/// Mutable two-tier metrics collector with rolling percentiles.
#[derive(Debug, Clone)]
pub struct TwoTierMetrics {
    init: Option<TwoTierInitMetrics>,
    search: Option<TwoTierSearchMetrics>,
    index: TwoTierIndexMetrics,
    aggregated: TwoTierAggregatedMetrics,
    fast_latency_window: VecDeque<u64>,
    total_latency_window: VecDeque<u64>,
    latency_window_size: usize,
}

impl Default for TwoTierMetrics {
    fn default() -> Self {
        Self::new(DEFAULT_LATENCY_WINDOW_SIZE)
    }
}

impl TwoTierMetrics {
    /// Create a collector with a fixed rolling window size.
    #[must_use]
    pub fn new(latency_window_size: usize) -> Self {
        Self {
            init: None,
            search: None,
            index: TwoTierIndexMetrics::default(),
            aggregated: TwoTierAggregatedMetrics::default(),
            fast_latency_window: VecDeque::new(),
            total_latency_window: VecDeque::new(),
            latency_window_size: latency_window_size.max(1),
        }
    }

    /// Record initialization metrics.
    pub const fn record_init(&mut self, metrics: TwoTierInitMetrics) {
        self.init = Some(metrics);
    }

    /// Record latest index metrics.
    pub const fn record_index(&mut self, metrics: TwoTierIndexMetrics) {
        self.index = metrics;
    }

    /// Record per-query metrics and update rolling aggregates.
    pub fn record_search(&mut self, metrics: TwoTierSearchMetrics) {
        let fast_latency = metrics.fast_latency_us();
        let total_latency = metrics.total_latency_us();
        let was_refined = metrics.was_refined;
        let ranking_changed = metrics.ranking_changed;

        self.search = Some(metrics);
        saturating_increment(&mut self.aggregated.total_searches, 1, "total_searches");
        if was_refined {
            saturating_increment(&mut self.aggregated.refined_searches, 1, "refined_searches");
        }
        if ranking_changed {
            saturating_increment(
                &mut self.aggregated.ranking_changed_count,
                1,
                "ranking_changed_count",
            );
        }

        push_window(
            &mut self.fast_latency_window,
            fast_latency,
            self.latency_window_size,
        );
        push_window(
            &mut self.total_latency_window,
            total_latency,
            self.latency_window_size,
        );

        self.aggregated.fast_latency_p50_us =
            percentile_nearest_rank(&self.fast_latency_window, 50);
        self.aggregated.fast_latency_p95_us =
            percentile_nearest_rank(&self.fast_latency_window, 95);
        self.aggregated.total_latency_p50_us =
            percentile_nearest_rank(&self.total_latency_window, 50);
        self.aggregated.total_latency_p95_us =
            percentile_nearest_rank(&self.total_latency_window, 95);
    }

    /// Produce a read-only snapshot.
    #[must_use]
    pub fn snapshot(&self) -> TwoTierMetricsSnapshot {
        TwoTierMetricsSnapshot {
            init: self.init.clone(),
            search: self.search.clone(),
            index: self.index.clone(),
            aggregated: self.aggregated.clone(),
        }
    }

    /// Check alerts and emit structured warnings on threshold breaches.
    #[must_use]
    pub fn check_alerts(&self, config: &TwoTierAlertConfig) -> TwoTierAlertState {
        let mut state = TwoTierAlertState::default();

        if let Some(search) = self.search.as_ref() {
            if search.fast_search_us > config.fast_latency_warn_us {
                state.slow_fast_search = true;
                warn!(
                    target: "search.two_tier.alert",
                    fast_latency_us = search.fast_search_us,
                    threshold_us = config.fast_latency_warn_us,
                    query_hash = search.query_hash,
                    "Fast search latency exceeded threshold"
                );
            }

            let quality_latency = search.quality_latency_us();
            if quality_latency > config.quality_latency_warn_us {
                state.slow_quality_refinement = true;
                warn!(
                    target: "search.two_tier.alert",
                    quality_latency_us = quality_latency,
                    threshold_us = config.quality_latency_warn_us,
                    query_hash = search.query_hash,
                    "Quality refinement latency exceeded threshold"
                );
            }
        }

        if self.index.quality_coverage < (config.quality_coverage_warn_pct / 100.0) {
            state.low_quality_coverage = true;
            warn!(
                target: "search.two_tier.alert",
                coverage_pct = self.index.quality_coverage * 100.0,
                threshold_pct = config.quality_coverage_warn_pct,
                doc_count = self.index.doc_count,
                quality_doc_count = self.index.quality_doc_count,
                "Quality embedding coverage below threshold"
            );
        }

        if self.index.doc_count > config.index_size_warn_docs {
            state.large_index = true;
            warn!(
                target: "search.two_tier.alert",
                doc_count = self.index.doc_count,
                threshold_docs = config.index_size_warn_docs,
                "Two-tier index size exceeded threshold"
            );
        }

        if self.index.total_memory_bytes > config.memory_warn_bytes {
            state.high_memory = true;
            warn!(
                target: "search.two_tier.alert",
                total_memory_bytes = self.index.total_memory_bytes,
                threshold_bytes = config.memory_warn_bytes,
                "Two-tier index memory usage exceeded threshold"
            );
        }

        state
    }
}

fn push_window(window: &mut VecDeque<u64>, value: u64, max_len: usize) {
    window.push_back(value);
    while window.len() > max_len {
        window.pop_front();
    }
}

fn percentile_nearest_rank(window: &VecDeque<u64>, percentile: usize) -> u64 {
    if window.is_empty() {
        return 0;
    }

    let mut sorted = window.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();

    let len = sorted.len();
    let rank = (len * percentile).div_ceil(100);
    let idx = rank.saturating_sub(1).min(len.saturating_sub(1));
    sorted[idx]
}

fn saturating_increment(counter: &mut u64, delta: u64, counter_name: &str) {
    let before = *counter;
    *counter = counter.saturating_add(delta);
    if before < COUNTER_WARN_THRESHOLD && *counter >= COUNTER_WARN_THRESHOLD {
        warn!(
            target: "search.two_tier.alert",
            counter = counter_name,
            value = *counter,
            threshold = COUNTER_WARN_THRESHOLD,
            "Two-tier counter approaching saturation"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use half::f16;
    use tracing::{Event, Id, Metadata, Subscriber, span, subscriber::Interest};

    use super::*;
    use crate::document::DocKind;
    use crate::two_tier::{
        TwoTierConfig, TwoTierEmbedder, TwoTierEntry, TwoTierIndex, TwoTierSearcher,
    };

    #[test]
    fn test_init_metrics_captured() {
        let mut metrics = TwoTierMetrics::default();
        let init = TwoTierInitMetrics {
            init_timestamp: 1_700_000_000,
            fast_embedder_load_ms: 25,
            quality_embedder_load_ms: 180,
            availability: TwoTierAvailability::Full,
            init_attempts: 1,
        };
        metrics.record_init(init.clone());

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.init, Some(init));
    }

    #[test]
    fn test_search_metrics_timing() {
        let mut metrics = TwoTierMetrics::default();
        let mut search = TwoTierSearchMetrics::for_query("hello world");
        search.fast_embed_us = 120;
        search.fast_search_us = 280;
        search.quality_embed_us = 1_200;
        search.quality_score_us = 900;
        search.blend_us = 110;
        search.fast_candidate_count = 12;
        search.refined_count = 8;
        search.was_refined = true;
        search.ranking_changed = true;

        metrics.record_search(search.clone());
        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.search, Some(search));
        assert_eq!(snapshot.aggregated.total_searches, 1);
        assert_eq!(snapshot.aggregated.refined_searches, 1);
        assert_eq!(snapshot.aggregated.ranking_changed_count, 1);
        assert!(snapshot.aggregated.fast_latency_p50_us > 0);
        assert!(snapshot.aggregated.total_latency_p50_us > 0);
    }

    #[test]
    fn test_index_metrics_coverage() {
        let index = TwoTierIndexMetrics::from_counts(10, 7, 10_000, 20_000);
        assert_eq!(index.doc_count, 10);
        assert_eq!(index.quality_doc_count, 7);
        assert!((index.quality_coverage - 0.7).abs() < f32::EPSILON);
        assert_eq!(index.total_memory_bytes, 30_000);
    }

    #[test]
    fn test_aggregated_metrics_percentiles() {
        let mut metrics = TwoTierMetrics::new(32);
        for latency in [10_u64, 20, 30, 40, 50] {
            let mut search = TwoTierSearchMetrics::for_query("p50-p95");
            search.fast_embed_us = latency;
            search.fast_search_us = 0;
            search.quality_embed_us = latency * 2;
            metrics.record_search(search);
        }

        let aggregated = metrics.snapshot().aggregated;
        assert_eq!(aggregated.fast_latency_p50_us, 30);
        assert_eq!(aggregated.fast_latency_p95_us, 50);
        assert_eq!(aggregated.total_latency_p50_us, 90);
        assert_eq!(aggregated.total_latency_p95_us, 150);
    }

    #[test]
    fn test_alert_on_slow_fast_search() {
        let mut metrics = TwoTierMetrics::default();
        let mut search = TwoTierSearchMetrics::for_query("slow-fast");
        search.fast_search_us = 6_000;
        metrics.record_search(search);

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert!(state.slow_fast_search);
    }

    #[test]
    fn test_alert_on_low_quality_coverage() {
        let mut metrics = TwoTierMetrics::default();
        metrics.record_index(TwoTierIndexMetrics::from_counts(100, 20, 100, 100));

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert!(state.low_quality_coverage);
    }

    #[test]
    fn test_alert_on_large_index() {
        let mut metrics = TwoTierMetrics::default();
        metrics.record_index(TwoTierIndexMetrics::from_counts(90_001, 90_001, 100, 100));

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert!(state.large_index);
    }

    #[test]
    fn test_no_alert_when_within_thresholds() {
        let mut metrics = TwoTierMetrics::default();
        let mut search = TwoTierSearchMetrics::for_query("healthy");
        search.fast_search_us = 1_000;
        search.quality_embed_us = 10_000;
        search.quality_score_us = 20_000;
        search.blend_us = 5_000;
        metrics.record_search(search);
        metrics.record_index(TwoTierIndexMetrics::from_counts(
            1_000, 800, 8_000_000, 16_000_000,
        ));

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert_eq!(state, TwoTierAlertState::default());
    }

    #[derive(Clone, Default)]
    struct SpanCapture {
        names: Arc<Mutex<Vec<String>>>,
        next_id: Arc<AtomicU64>,
    }

    impl SpanCapture {
        fn names(&self) -> Vec<String> {
            self.names.lock().expect("span lock poisoned").clone()
        }
    }

    impl Subscriber for SpanCapture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }

        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }

        fn new_span(&self, attrs: &span::Attributes<'_>) -> Id {
            self.names
                .lock()
                .expect("span lock poisoned")
                .push(attrs.metadata().name().to_string());
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            Id::from_u64(id)
        }

        fn record(&self, _span: &Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, _event: &Event<'_>) {}

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}

        fn clone_span(&self, id: &Id) -> Id {
            id.clone()
        }

        fn try_close(&self, _id: Id) -> bool {
            true
        }
    }

    struct FixedEmbedder {
        id: &'static str,
        vector: Vec<f32>,
    }

    impl FixedEmbedder {
        fn new(id: &'static str, vector: Vec<f32>) -> Self {
            Self { id, vector }
        }
    }

    impl TwoTierEmbedder for FixedEmbedder {
        fn embed(&self, _text: &str) -> crate::SearchResult<Vec<f32>> {
            Ok(self.vector.clone())
        }

        fn dimension(&self) -> usize {
            self.vector.len()
        }

        fn id(&self) -> &str {
            self.id
        }
    }

    #[test]
    fn test_tracing_spans_complete() {
        let capture = SpanCapture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            let _init_span = tracing::info_span!("two_tier.init").entered();
            let config = TwoTierConfig {
                fast_dimension: 2,
                quality_dimension: 2,
                ..TwoTierConfig::default()
            };
            let mut index = TwoTierIndex::new(&config);
            index
                .add_entry(TwoTierEntry {
                    doc_id: 1,
                    doc_kind: DocKind::Message,
                    project_id: Some(1),
                    fast_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                    quality_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                    has_quality: true,
                })
                .expect("entry should be valid");

            let searcher = TwoTierSearcher::new(
                &index,
                Arc::new(FixedEmbedder::new("fast", vec![1.0, 0.0])),
                Some(Arc::new(FixedEmbedder::new("quality", vec![1.0, 0.0]))),
                config,
            );
            // Consume ALL phases so every span fires (fast + refinement).
            let phase_count = searcher.search("hello", 1).count();
            assert!(phase_count > 0);
        });

        let names = capture.names();
        for required in REQUIRED_TWO_TIER_SPANS {
            assert!(
                names.iter().any(|name| name == required),
                "missing required span {required}; got {names:?}"
            );
        }
    }

    // -- percentile_nearest_rank edge cases --

    #[test]
    fn percentile_empty_window_returns_zero() {
        let window = VecDeque::new();
        assert_eq!(percentile_nearest_rank(&window, 50), 0);
        assert_eq!(percentile_nearest_rank(&window, 95), 0);
    }

    #[test]
    fn percentile_single_element() {
        let window = VecDeque::from([42]);
        assert_eq!(percentile_nearest_rank(&window, 0), 42);
        assert_eq!(percentile_nearest_rank(&window, 50), 42);
        assert_eq!(percentile_nearest_rank(&window, 100), 42);
    }

    #[test]
    fn percentile_two_elements() {
        let window = VecDeque::from([10, 90]);
        assert_eq!(percentile_nearest_rank(&window, 50), 10);
        assert_eq!(percentile_nearest_rank(&window, 51), 90);
        assert_eq!(percentile_nearest_rank(&window, 100), 90);
    }

    #[test]
    fn percentile_sorted_and_unsorted_same_result() {
        let sorted = VecDeque::from([1, 2, 3, 4, 5]);
        let unsorted = VecDeque::from([5, 3, 1, 4, 2]);
        assert_eq!(
            percentile_nearest_rank(&sorted, 50),
            percentile_nearest_rank(&unsorted, 50)
        );
        assert_eq!(
            percentile_nearest_rank(&sorted, 95),
            percentile_nearest_rank(&unsorted, 95)
        );
    }

    // -- push_window --

    #[test]
    fn push_window_respects_max_len() {
        let mut window = VecDeque::new();
        for i in 0..10 {
            push_window(&mut window, i, 5);
        }
        assert_eq!(window.len(), 5);
        // Should contain last 5 values
        assert_eq!(window, VecDeque::from([5, 6, 7, 8, 9]));
    }

    #[test]
    fn push_window_single_capacity() {
        let mut window = VecDeque::new();
        push_window(&mut window, 1, 1);
        push_window(&mut window, 2, 1);
        push_window(&mut window, 3, 1);
        assert_eq!(window.len(), 1);
        assert_eq!(window[0], 3);
    }

    // -- TwoTierSearchMetrics computed latencies --

    #[test]
    fn search_metrics_latency_accessors() {
        let m = TwoTierSearchMetrics {
            fast_embed_us: 100,
            fast_search_us: 200,
            quality_embed_us: 1_000,
            quality_score_us: 500,
            blend_us: 50,
            ..TwoTierSearchMetrics::default()
        };
        assert_eq!(m.fast_latency_us(), 300);
        assert_eq!(m.quality_latency_us(), 1_550);
        assert_eq!(m.total_latency_us(), 1_850);
    }

    #[test]
    fn search_metrics_default_all_zero() {
        let m = TwoTierSearchMetrics::default();
        assert_eq!(m.fast_latency_us(), 0);
        assert_eq!(m.quality_latency_us(), 0);
        assert_eq!(m.total_latency_us(), 0);
        assert!(!m.was_refined);
        assert!(!m.ranking_changed);
    }

    #[test]
    fn search_metrics_for_query_deterministic() {
        let m1 = TwoTierSearchMetrics::for_query("hello world");
        let m2 = TwoTierSearchMetrics::for_query("hello world");
        assert_eq!(m1.query_hash, m2.query_hash);

        let m3 = TwoTierSearchMetrics::for_query("different query");
        assert_ne!(m1.query_hash, m3.query_hash);
    }

    // -- TwoTierIndexMetrics --

    #[test]
    fn index_metrics_zero_docs_coverage_one() {
        let index = TwoTierIndexMetrics::from_counts(0, 0, 0, 0);
        assert!((index.quality_coverage - 1.0).abs() < f32::EPSILON);
        assert_eq!(index.total_memory_bytes, 0);
    }

    #[test]
    fn index_metrics_memory_saturating() {
        let index = TwoTierIndexMetrics::from_counts(1, 1, usize::MAX, 1);
        assert_eq!(index.total_memory_bytes, usize::MAX);
    }

    #[test]
    fn index_metrics_default_all_zero() {
        let index = TwoTierIndexMetrics::default();
        assert_eq!(index.doc_count, 0);
        assert_eq!(index.quality_doc_count, 0);
        assert_eq!(index.total_memory_bytes, 0);
    }

    // -- TwoTierAlertConfig defaults --

    #[test]
    fn alert_config_defaults_reasonable() {
        let config = TwoTierAlertConfig::default();
        assert_eq!(config.fast_latency_warn_us, 5_000);
        assert_eq!(config.quality_latency_warn_us, 300_000);
        assert!((config.quality_coverage_warn_pct - 50.0).abs() < f32::EPSILON);
        assert_eq!(config.index_size_warn_docs, 80_000);
        assert_eq!(config.memory_warn_bytes, 500 * 1024 * 1024);
    }

    // -- TwoTierAlertState --

    #[test]
    fn alert_state_default_all_false() {
        let state = TwoTierAlertState::default();
        assert!(!state.slow_fast_search);
        assert!(!state.slow_quality_refinement);
        assert!(!state.low_quality_coverage);
        assert!(!state.large_index);
        assert!(!state.high_memory);
    }

    // -- check_alerts additional scenarios --

    #[test]
    fn alert_slow_quality_refinement() {
        let mut metrics = TwoTierMetrics::default();
        let mut search = TwoTierSearchMetrics::for_query("slow-quality");
        search.quality_embed_us = 200_000;
        search.quality_score_us = 150_000;
        metrics.record_search(search);

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert!(state.slow_quality_refinement);
        assert!(!state.slow_fast_search); // fast latency is fine
    }

    #[test]
    fn alert_high_memory() {
        let mut metrics = TwoTierMetrics::default();
        metrics.record_index(TwoTierIndexMetrics::from_counts(
            1_000,
            1_000,
            300 * 1024 * 1024,
            300 * 1024 * 1024,
        ));

        let state = metrics.check_alerts(&TwoTierAlertConfig::default());
        assert!(state.high_memory);
    }

    #[test]
    fn alert_custom_thresholds() {
        let mut metrics = TwoTierMetrics::default();
        let mut search = TwoTierSearchMetrics::for_query("custom");
        search.fast_search_us = 100; // Very fast
        metrics.record_search(search);

        // Strict threshold: 50µs
        let config = TwoTierAlertConfig {
            fast_latency_warn_us: 50,
            ..TwoTierAlertConfig::default()
        };
        let state = metrics.check_alerts(&config);
        assert!(state.slow_fast_search);
    }

    // -- TwoTierMetrics window and recording --

    #[test]
    fn metrics_new_min_window_size() {
        // Window size 0 should be promoted to 1
        let metrics = TwoTierMetrics::new(0);
        assert_eq!(metrics.latency_window_size, 1);
    }

    #[test]
    fn record_search_no_refinement() {
        let mut metrics = TwoTierMetrics::default();
        let search = TwoTierSearchMetrics {
            fast_embed_us: 100,
            fast_search_us: 200,
            was_refined: false,
            ranking_changed: false,
            ..TwoTierSearchMetrics::default()
        };
        metrics.record_search(search);

        let snap = metrics.snapshot();
        assert_eq!(snap.aggregated.total_searches, 1);
        assert_eq!(snap.aggregated.refined_searches, 0);
        assert_eq!(snap.aggregated.ranking_changed_count, 0);
    }

    #[test]
    fn snapshot_initial_state() {
        let metrics = TwoTierMetrics::default();
        let snap = metrics.snapshot();
        assert!(snap.init.is_none());
        assert!(snap.search.is_none());
        assert_eq!(snap.index, TwoTierIndexMetrics::default());
        assert_eq!(snap.aggregated, TwoTierAggregatedMetrics::default());
    }

    #[test]
    fn aggregated_metrics_default_all_zero() {
        let agg = TwoTierAggregatedMetrics::default();
        assert_eq!(agg.total_searches, 0);
        assert_eq!(agg.refined_searches, 0);
        assert_eq!(agg.ranking_changed_count, 0);
        assert_eq!(agg.fast_latency_p50_us, 0);
        assert_eq!(agg.fast_latency_p95_us, 0);
    }

    // -- Serde roundtrips --

    #[test]
    fn search_metrics_serde_roundtrip() {
        let m = TwoTierSearchMetrics {
            query_hash: 12345,
            fast_embed_us: 100,
            fast_search_us: 200,
            quality_embed_us: 1_000,
            quality_score_us: 500,
            blend_us: 50,
            fast_candidate_count: 20,
            refined_count: 10,
            was_refined: true,
            ranking_changed: true,
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: TwoTierSearchMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, m);
    }

    #[test]
    fn alert_state_serde_roundtrip() {
        let state = TwoTierAlertState {
            slow_fast_search: true,
            slow_quality_refinement: false,
            low_quality_coverage: true,
            large_index: false,
            high_memory: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: TwoTierAlertState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn metrics_snapshot_serde_roundtrip() {
        let snap = TwoTierMetricsSnapshot {
            init: Some(TwoTierInitMetrics {
                init_timestamp: 1_700_000_000,
                fast_embedder_load_ms: 25,
                quality_embedder_load_ms: 180,
                availability: TwoTierAvailability::Full,
                init_attempts: 1,
            }),
            search: None,
            index: TwoTierIndexMetrics::from_counts(100, 80, 1_000, 2_000),
            aggregated: TwoTierAggregatedMetrics::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored: TwoTierMetricsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.init, snap.init);
        assert_eq!(restored.aggregated, snap.aggregated);
    }
}
