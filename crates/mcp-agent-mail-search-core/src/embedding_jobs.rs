//! Background embedding and index refresh jobs.
//!
//! This module provides batch scheduling, incremental updates, and retry/failure
//! bookkeeping for the semantic search embedding pipeline.
//!
//! # Architecture
//!
//! The embedding job system has three main components:
//!
//! 1. **`EmbeddingQueue`**: Collects pending embedding requests with backpressure
//! 2. **`EmbeddingJobRunner`**: Processes batches with retry logic and metrics
//! 3. **`IndexRefreshWorker`**: Background thread that drives the refresh loop
//!
//! # Workflow
//!
//! ```text
//! Write event → EmbeddingQueue.enqueue() → JobRunner.process_batch()
//!                                              ↓
//!                               Embedder.embed_batch() → VectorIndex.upsert()
//!                                              ↓
//!                               EmbeddingsDB.persist() → JobMetrics.record()
//! ```
//!
//! # Feature Gating
//!
//! This module is compiled when the `semantic` feature is enabled.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::canonical::{CanonPolicy, canonicalize_and_hash};
use crate::document::DocKind;
use crate::embedder::{Embedder, EmbeddingResult, ModelTier};
use crate::engine::IndexLifecycle;
use crate::error::SearchResult;
use crate::vector_index::{IndexEntry, VectorIndex, VectorMetadata};

// ────────────────────────────────────────────────────────────────────
// Configuration
// ────────────────────────────────────────────────────────────────────

/// Configuration for embedding jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingJobConfig {
    /// Maximum number of documents to embed in a single batch
    pub batch_size: usize,
    /// Maximum time to wait before processing a partial batch (milliseconds)
    pub flush_interval_ms: u64,
    /// Maximum number of pending jobs before backpressure kicks in
    pub backpressure_threshold: usize,
    /// Maximum retries for failed embedding operations
    pub max_retries: u32,
    /// Base delay between retries (milliseconds), doubles on each retry
    pub retry_base_delay_ms: u64,
    /// Timeout for a single embedding operation (milliseconds)
    pub timeout_ms: u64,
    /// Whether to persist embeddings to the database
    pub persist_to_db: bool,
    /// Canonicalization policy for document text
    #[serde(skip)]
    pub canon_policy: CanonPolicy,
}

impl Default for EmbeddingJobConfig {
    fn default() -> Self {
        Self {
            batch_size: 32,
            flush_interval_ms: 5000,
            backpressure_threshold: 1000,
            max_retries: 3,
            retry_base_delay_ms: 100,
            timeout_ms: 30_000,
            persist_to_db: true,
            canon_policy: CanonPolicy::Full,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Embedding request
// ────────────────────────────────────────────────────────────────────

/// A request to embed a document.
#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    /// Document ID
    pub doc_id: i64,
    /// Document kind
    pub doc_kind: DocKind,
    /// Project ID (optional)
    pub project_id: Option<i64>,
    /// Document title
    pub title: String,
    /// Document body
    pub body: String,
    /// Requested model tier
    pub tier: ModelTier,
    /// Number of retry attempts so far
    pub retries: u32,
    /// When the request was enqueued
    pub enqueued_at: Instant,
    /// Earliest time this request may be retried.
    pub next_attempt_at: Instant,
}

impl EmbeddingRequest {
    /// Create a new embedding request.
    #[must_use]
    pub fn new(
        doc_id: i64,
        doc_kind: DocKind,
        project_id: Option<i64>,
        title: impl Into<String>,
        body: impl Into<String>,
        tier: ModelTier,
    ) -> Self {
        Self {
            doc_id,
            doc_kind,
            project_id,
            title: title.into(),
            body: body.into(),
            tier,
            retries: 0,
            enqueued_at: Instant::now(),
            next_attempt_at: Instant::now(),
        }
    }

    /// Create a key for deduplication.
    #[must_use]
    pub const fn dedup_key(&self) -> (i64, DocKind) {
        (self.doc_id, self.doc_kind)
    }
}

// ────────────────────────────────────────────────────────────────────
// Job result
// ────────────────────────────────────────────────────────────────────

/// Result of processing a single embedding request.
#[derive(Debug, Clone)]
pub enum JobResult {
    /// Successfully embedded
    Success {
        doc_id: i64,
        doc_kind: DocKind,
        model_id: String,
        content_hash: String,
        dimension: usize,
        elapsed: Duration,
    },
    /// Failed but may retry
    Retryable {
        doc_id: i64,
        doc_kind: DocKind,
        error: String,
        retries: u32,
    },
    /// Permanently failed
    Failed {
        doc_id: i64,
        doc_kind: DocKind,
        error: String,
    },
    /// Skipped (duplicate, already up-to-date)
    Skipped {
        doc_id: i64,
        doc_kind: DocKind,
        reason: String,
    },
}

impl JobResult {
    /// Check if this result indicates success.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// Check if this result should be retried.
    #[must_use]
    pub const fn should_retry(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }
}

// ────────────────────────────────────────────────────────────────────
// Batch result
// ────────────────────────────────────────────────────────────────────

/// Statistics from processing a batch of embedding requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchResult {
    /// Number of successful embeddings
    pub succeeded: usize,
    /// Number of retryable failures
    pub retryable: usize,
    /// Number of permanent failures
    pub failed: usize,
    /// Number of skipped documents
    pub skipped: usize,
    /// Total processing time
    pub elapsed: Duration,
    /// Per-document results (optional, for debugging)
    #[serde(skip)]
    pub details: Vec<JobResult>,
}

impl BatchResult {
    /// Total number of documents processed.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.succeeded + self.retryable + self.failed + self.skipped
    }
}

// ────────────────────────────────────────────────────────────────────
// Embedding queue
// ────────────────────────────────────────────────────────────────────

/// Thread-safe queue for pending embedding requests with backpressure.
pub struct EmbeddingQueue {
    config: EmbeddingJobConfig,
    pending: Mutex<QueueState>,
}

struct QueueState {
    /// Pending requests (FIFO)
    queue: VecDeque<EmbeddingRequest>,
    /// Dedup set of pending keys across main + retry queues
    dedup: HashSet<(i64, DocKind)>,
    /// Requests pending retry
    retry_queue: VecDeque<EmbeddingRequest>,
    /// Statistics
    stats: QueueStats,
}

/// Statistics about the embedding queue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStats {
    /// Number of requests currently pending
    pub pending_count: usize,
    /// Number of requests in retry queue
    pub retry_count: usize,
    /// Total requests enqueued since start
    pub total_enqueued: u64,
    /// Total requests dropped due to backpressure
    pub total_dropped: u64,
    /// Total requests deduplicated
    pub total_deduped: u64,
}

impl EmbeddingQueue {
    /// Create a new embedding queue with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(EmbeddingJobConfig::default())
    }

    /// Create a new embedding queue with custom configuration.
    #[must_use]
    pub fn with_config(config: EmbeddingJobConfig) -> Self {
        Self {
            config,
            pending: Mutex::new(QueueState {
                queue: VecDeque::new(),
                dedup: HashSet::new(),
                retry_queue: VecDeque::new(),
                stats: QueueStats::default(),
            }),
        }
    }

    /// Enqueue an embedding request.
    ///
    /// Returns `true` if the request was accepted, `false` if dropped due to
    /// backpressure.
    pub fn enqueue(&self, request: EmbeddingRequest) -> bool {
        let mut state = self.pending.lock().expect("queue lock poisoned");

        // Check backpressure
        let total_pending = state.queue.len() + state.retry_queue.len();
        if total_pending >= self.config.backpressure_threshold {
            state.stats.total_dropped += 1;
            return false;
        }

        // Check dedup
        let key = request.dedup_key();
        if state.dedup.contains(&key) {
            if let Some(existing) = state
                .queue
                .iter_mut()
                .find(|pending| pending.dedup_key() == key)
            {
                *existing = request;
                state.stats.total_deduped += 1;
                state.stats.pending_count = state.queue.len();
                state.stats.retry_count = state.retry_queue.len();
                return true;
            }
            if let Some(existing) = state
                .retry_queue
                .iter_mut()
                .find(|pending| pending.dedup_key() == key)
            {
                *existing = request;
                state.stats.total_deduped += 1;
                state.stats.pending_count = state.queue.len();
                state.stats.retry_count = state.retry_queue.len();
                return true;
            }
            // Stale dedup key; clear and continue with enqueue.
            state.dedup.remove(&key);
        }

        // Add to queue
        state.dedup.insert(key);
        state.queue.push_back(request);
        state.stats.total_enqueued += 1;
        state.stats.pending_count = state.queue.len();
        state.stats.retry_count = state.retry_queue.len();

        true
    }

    /// Enqueue a request for retry (goes to retry queue).
    pub fn enqueue_retry(&self, mut request: EmbeddingRequest) {
        request.retries += 1;
        let mut state = self.pending.lock().expect("queue lock poisoned");
        let key = request.dedup_key();
        if state.dedup.contains(&key) {
            state.stats.total_deduped += 1;
            state.stats.pending_count = state.queue.len();
            state.stats.retry_count = state.retry_queue.len();
            return;
        }
        let shift = request.retries.saturating_sub(1).min(20);
        let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay_ms = self.config.retry_base_delay_ms.saturating_mul(factor);
        let delay = Duration::from_millis(delay_ms);
        request.next_attempt_at = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
        state.dedup.insert(key);
        state.retry_queue.push_back(request);
        state.stats.pending_count = state.queue.len();
        state.stats.retry_count = state.retry_queue.len();
    }

    /// Drain up to `batch_size` requests from the queue.
    ///
    /// Retry queue is drained first, then main queue.
    pub fn drain_batch(&self, batch_size: usize) -> Vec<EmbeddingRequest> {
        let mut state = self.pending.lock().expect("queue lock poisoned");
        let mut batch = Vec::with_capacity(batch_size);
        let now = Instant::now();

        // Drain retry queue first (priority)
        let mut deferred_retry = VecDeque::with_capacity(state.retry_queue.len());
        while let Some(req) = state.retry_queue.pop_front() {
            if batch.len() < batch_size && req.next_attempt_at <= now {
                state.dedup.remove(&req.dedup_key());
                batch.push(req);
            } else {
                deferred_retry.push_back(req);
            }
        }
        state.retry_queue = deferred_retry;

        // Then main queue
        while batch.len() < batch_size && !state.queue.is_empty() {
            if let Some(req) = state.queue.pop_front() {
                state.dedup.remove(&req.dedup_key());
                batch.push(req);
            }
        }

        // Update stats
        state.stats.pending_count = state.queue.len();
        state.stats.retry_count = state.retry_queue.len();

        batch
    }

    /// Check if the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let state = self.pending.lock().expect("queue lock poisoned");
        state.queue.is_empty() && state.retry_queue.is_empty()
    }

    /// Get current queue length (main + retry).
    #[must_use]
    pub fn len(&self) -> usize {
        let state = self.pending.lock().expect("queue lock poisoned");
        state.queue.len() + state.retry_queue.len()
    }

    /// Get queue statistics.
    #[must_use]
    pub fn stats(&self) -> QueueStats {
        let state = self.pending.lock().expect("queue lock poisoned");
        QueueStats {
            pending_count: state.queue.len(),
            retry_count: state.retry_queue.len(),
            ..state.stats.clone()
        }
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &EmbeddingJobConfig {
        &self.config
    }
}

impl Default for EmbeddingQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────────
// Job metrics
// ────────────────────────────────────────────────────────────────────

/// Metrics for embedding job processing.
#[derive(Debug, Default)]
pub struct JobMetrics {
    /// Total successful embeddings
    pub total_succeeded: AtomicU64,
    /// Total retryable failures
    pub total_retryable: AtomicU64,
    /// Total permanent failures
    pub total_failed: AtomicU64,
    /// Total skipped documents
    pub total_skipped: AtomicU64,
    /// Total batches processed
    pub total_batches: AtomicU64,
    /// Total embedding time (microseconds)
    pub total_embed_time_us: AtomicU64,
    /// Total documents embedded
    pub total_docs_embedded: AtomicU64,
}

impl JobMetrics {
    /// Create new metrics.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            total_succeeded: AtomicU64::new(0),
            total_retryable: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
            total_skipped: AtomicU64::new(0),
            total_batches: AtomicU64::new(0),
            total_embed_time_us: AtomicU64::new(0),
            total_docs_embedded: AtomicU64::new(0),
        }
    }

    /// Record a batch result.
    pub fn record_batch(&self, result: &BatchResult) {
        self.total_succeeded
            .fetch_add(result.succeeded as u64, Ordering::Relaxed);
        self.total_retryable
            .fetch_add(result.retryable as u64, Ordering::Relaxed);
        self.total_failed
            .fetch_add(result.failed as u64, Ordering::Relaxed);
        self.total_skipped
            .fetch_add(result.skipped as u64, Ordering::Relaxed);
        self.total_batches.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)]
        self.total_embed_time_us
            .fetch_add(result.elapsed.as_micros() as u64, Ordering::Relaxed);
        self.total_docs_embedded
            .fetch_add(result.succeeded as u64, Ordering::Relaxed);
    }

    /// Get a snapshot of the metrics.
    #[must_use]
    pub fn snapshot(&self) -> JobMetricsSnapshot {
        JobMetricsSnapshot {
            total_succeeded: self.total_succeeded.load(Ordering::Relaxed),
            total_retryable: self.total_retryable.load(Ordering::Relaxed),
            total_failed: self.total_failed.load(Ordering::Relaxed),
            total_skipped: self.total_skipped.load(Ordering::Relaxed),
            total_batches: self.total_batches.load(Ordering::Relaxed),
            total_embed_time_us: self.total_embed_time_us.load(Ordering::Relaxed),
            total_docs_embedded: self.total_docs_embedded.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of job metrics (serializable).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobMetricsSnapshot {
    pub total_succeeded: u64,
    pub total_retryable: u64,
    pub total_failed: u64,
    pub total_skipped: u64,
    pub total_batches: u64,
    pub total_embed_time_us: u64,
    pub total_docs_embedded: u64,
}

impl JobMetricsSnapshot {
    /// Average embedding time per document (microseconds).
    #[must_use]
    pub fn avg_embed_time_us(&self) -> u64 {
        self.total_embed_time_us
            .checked_div(self.total_docs_embedded)
            .unwrap_or(0)
    }
}

// ────────────────────────────────────────────────────────────────────
// Job runner
// ────────────────────────────────────────────────────────────────────

/// Processes embedding requests in batches.
pub struct EmbeddingJobRunner {
    config: EmbeddingJobConfig,
    queue: Arc<EmbeddingQueue>,
    embedder: Arc<dyn Embedder>,
    index: Arc<RwLock<VectorIndex>>,
    metrics: Arc<JobMetrics>,
}

impl EmbeddingJobRunner {
    /// Create a new job runner.
    #[must_use]
    pub fn new(
        config: EmbeddingJobConfig,
        queue: Arc<EmbeddingQueue>,
        embedder: Arc<dyn Embedder>,
        index: Arc<RwLock<VectorIndex>>,
    ) -> Self {
        Self {
            config,
            queue,
            embedder,
            index,
            metrics: Arc::new(JobMetrics::new()),
        }
    }

    /// Get the metrics.
    #[must_use]
    pub fn metrics(&self) -> Arc<JobMetrics> {
        self.metrics.clone()
    }

    /// Process a single batch of requests.
    ///
    /// Returns the batch result and any requests that should be retried.
    pub fn process_batch(&self) -> SearchResult<BatchResult> {
        self.process_batch_limit(self.config.batch_size)
    }

    /// Process at most `batch_size` requests from the queue.
    ///
    /// This is used by the refresh worker to enforce per-cycle processing bounds.
    pub fn process_batch_limit(&self, batch_size: usize) -> SearchResult<BatchResult> {
        let effective_batch_size = batch_size.max(1).min(self.config.batch_size);
        let batch = self.queue.drain_batch(effective_batch_size);
        if batch.is_empty() {
            return Ok(BatchResult::default());
        }

        let start = Instant::now();
        let mut result = BatchResult::default();

        // Prepare texts for batch embedding
        let texts: Vec<String> = batch
            .iter()
            .map(|req| {
                let (canonical, _hash) = canonicalize_and_hash(
                    req.doc_kind,
                    &req.title,
                    &req.body,
                    self.config.canon_policy,
                );
                canonical
            })
            .collect();

        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();

        // Batch embed
        let embeddings = match self.embedder.embed_batch(&text_refs) {
            Ok(embs) => embs,
            Err(e) => {
                // Batch failed, mark all as retryable
                for req in batch {
                    if req.retries < self.config.max_retries {
                        self.queue.enqueue_retry(req.clone());
                        result.retryable += 1;
                        result.details.push(JobResult::Retryable {
                            doc_id: req.doc_id,
                            doc_kind: req.doc_kind,
                            error: e.to_string(),
                            retries: req.retries + 1,
                        });
                    } else {
                        result.failed += 1;
                        result.details.push(JobResult::Failed {
                            doc_id: req.doc_id,
                            doc_kind: req.doc_kind,
                            error: e.to_string(),
                        });
                    }
                }
                result.elapsed = start.elapsed();
                self.metrics.record_batch(&result);
                return Ok(result);
            }
        };

        // Process each embedding
        let mut index = self.index.write().expect("index lock poisoned");

        for (req, embedding) in batch.iter().zip(embeddings.into_iter()) {
            match self.process_single(&mut index, req, embedding) {
                Ok(job_result) => {
                    match &job_result {
                        JobResult::Success { .. } => result.succeeded += 1,
                        JobResult::Retryable { .. } => result.retryable += 1,
                        JobResult::Failed { .. } => result.failed += 1,
                        JobResult::Skipped { .. } => result.skipped += 1,
                    }
                    result.details.push(job_result);
                }
                Err(e) => {
                    result.failed += 1;
                    result.details.push(JobResult::Failed {
                        doc_id: 0,
                        doc_kind: DocKind::Message,
                        error: e.to_string(),
                    });
                }
            }
        }

        result.elapsed = start.elapsed();
        self.metrics.record_batch(&result);

        Ok(result)
    }

    /// Process a single embedding request.
    #[allow(clippy::unused_self)]
    fn process_single(
        &self,
        index: &mut VectorIndex,
        req: &EmbeddingRequest,
        embedding: EmbeddingResult,
    ) -> SearchResult<JobResult> {
        // Skip hash-only embeddings for vector index
        if embedding.is_hash_only() {
            return Ok(JobResult::Skipped {
                doc_id: req.doc_id,
                doc_kind: req.doc_kind,
                reason: "hash-only embedding".to_owned(),
            });
        }

        // Build metadata
        let metadata = VectorMetadata::new(req.doc_id, req.doc_kind, &embedding.model_id)
            .with_hash(&embedding.content_hash);

        let metadata = if let Some(pid) = req.project_id {
            metadata.with_project(pid)
        } else {
            metadata
        };

        // Build index entry and upsert
        let entry = IndexEntry::new(&embedding.vector, metadata);
        index.upsert(entry)?;

        Ok(JobResult::Success {
            doc_id: req.doc_id,
            doc_kind: req.doc_kind,
            model_id: embedding.model_id,
            content_hash: embedding.content_hash,
            dimension: embedding.dimension,
            elapsed: embedding.elapsed,
        })
    }

    /// Check if there's work to do.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.queue.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────────
// Index refresh worker
// ────────────────────────────────────────────────────────────────────

/// Configuration for the index refresh worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshWorkerConfig {
    /// Interval between refresh cycles (milliseconds)
    pub refresh_interval_ms: u64,
    /// Whether to run a full rebuild on startup
    pub rebuild_on_startup: bool,
    /// Maximum documents to process in a single refresh cycle
    pub max_docs_per_cycle: usize,
}

impl Default for RefreshWorkerConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: 1000,
            rebuild_on_startup: false,
            max_docs_per_cycle: 1000,
        }
    }
}

/// Background worker that drives embedding refresh.
///
/// This is intentionally synchronous — it runs on a dedicated OS thread
/// with sleep-based iteration, matching the pattern in `cleanup.rs`.
pub struct IndexRefreshWorker {
    config: RefreshWorkerConfig,
    runner: Arc<EmbeddingJobRunner>,
    rebuild_lifecycle: Option<Arc<dyn IndexLifecycle>>,
    shutdown: Arc<AtomicBool>,
    last_refresh: Mutex<Option<Instant>>,
}

impl IndexRefreshWorker {
    /// Create a new refresh worker.
    #[must_use]
    pub fn new(config: RefreshWorkerConfig, runner: Arc<EmbeddingJobRunner>) -> Self {
        Self {
            config,
            runner,
            rebuild_lifecycle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            last_refresh: Mutex::new(None),
        }
    }

    /// Attach an optional index lifecycle for startup rebuild hooks.
    #[must_use]
    pub fn with_rebuild_lifecycle(mut self, lifecycle: Arc<dyn IndexLifecycle>) -> Self {
        self.rebuild_lifecycle = Some(lifecycle);
        self
    }

    /// Get the shutdown flag for external control.
    #[must_use]
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Signal the worker to stop.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Run the refresh loop (blocking).
    ///
    /// This should be called from a dedicated thread.
    pub fn run(&self) {
        let interval = Duration::from_millis(self.config.refresh_interval_ms.max(100));
        self.run_startup_rebuild();

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }

            // Process pending work
            if self.run_cycle() > 0 {
                *self.last_refresh.lock().expect("last_refresh lock") = Some(Instant::now());
            }

            // Sleep in small increments for responsive shutdown
            let mut remaining = interval;
            while !remaining.is_zero() {
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                let chunk = remaining.min(Duration::from_millis(100));
                std::thread::sleep(chunk);
                remaining = remaining.saturating_sub(chunk);
            }
        }
    }

    /// Get the metrics from the runner.
    #[must_use]
    pub fn metrics(&self) -> Arc<JobMetrics> {
        self.runner.metrics()
    }

    /// Get the last refresh time.
    #[must_use]
    pub fn last_refresh(&self) -> Option<Instant> {
        *self.last_refresh.lock().expect("last_refresh lock")
    }

    /// Process one bounded refresh cycle.
    ///
    /// Returns the number of documents processed this cycle.
    #[must_use]
    pub fn run_cycle(&self) -> usize {
        let max_docs = self.config.max_docs_per_cycle.max(1);
        let mut processed = 0usize;
        while self.runner.has_work() && processed < max_docs {
            let remaining = max_docs.saturating_sub(processed).max(1);
            match self.runner.process_batch_limit(remaining) {
                Ok(result) => {
                    let total = result.total();
                    if total == 0 {
                        break;
                    }
                    processed = processed.saturating_add(total);
                }
                Err(_e) => {
                    // Log error but continue on next cycle.
                    break;
                }
            }
        }
        processed
    }

    fn run_startup_rebuild(&self) {
        if !self.config.rebuild_on_startup {
            return;
        }
        if let Some(lifecycle) = self.rebuild_lifecycle.as_ref() {
            let _ = lifecycle.rebuild();
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Convenience: rebuild from source
// ────────────────────────────────────────────────────────────────────

/// Result of a full index rebuild.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RebuildResult {
    /// Total documents processed
    pub total_processed: usize,
    /// Successfully embedded
    pub succeeded: usize,
    /// Failed to embed
    pub failed: usize,
    /// Total time elapsed
    pub elapsed: Duration,
}

/// Rebuild progress callback.
pub trait RebuildProgress: Send + Sync {
    /// Called with progress updates.
    fn on_progress(&self, processed: usize, total: usize);
}

/// No-op progress reporter.
pub struct NoProgress;

impl RebuildProgress for NoProgress {
    #[allow(clippy::unused_self)]
    fn on_progress(&self, _processed: usize, _total: usize) {}
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{Embedder, HashEmbedder, ModelInfo};
    use crate::engine::{IndexHealth, IndexLifecycle, IndexStats};
    use crate::vector_index::VectorIndexConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_request(doc_id: i64) -> EmbeddingRequest {
        EmbeddingRequest::new(
            doc_id,
            DocKind::Message,
            Some(1),
            "Test Title",
            "Test body content",
            ModelTier::Fast,
        )
    }

    #[derive(Debug)]
    struct FixedEmbedder {
        info: ModelInfo,
    }

    impl FixedEmbedder {
        fn new(dimension: usize) -> Self {
            Self {
                info: ModelInfo::new("fixed-fast", "Fixed Fast", ModelTier::Fast, dimension, 4096)
                    .with_available(true),
            }
        }
    }

    impl Embedder for FixedEmbedder {
        fn embed(&self, text: &str) -> SearchResult<EmbeddingResult> {
            Ok(EmbeddingResult::new(
                vec![0.25_f32; self.info.dimension],
                self.info.id.clone(),
                ModelTier::Fast,
                Duration::from_millis(1),
                crate::canonical::content_hash(text),
            ))
        }

        fn model_info(&self) -> &ModelInfo {
            &self.info
        }
    }

    #[derive(Default)]
    struct MockLifecycle {
        rebuild_calls: AtomicUsize,
    }

    impl IndexLifecycle for MockLifecycle {
        fn rebuild(&self) -> SearchResult<IndexStats> {
            self.rebuild_calls.fetch_add(1, Ordering::Relaxed);
            Ok(IndexStats {
                docs_indexed: 0,
                docs_removed: 0,
                elapsed_ms: 0,
                warnings: Vec::new(),
            })
        }

        fn update_incremental(
            &self,
            changes: &[crate::document::DocChange],
        ) -> SearchResult<usize> {
            Ok(changes.len())
        }

        fn health(&self) -> IndexHealth {
            IndexHealth {
                ready: true,
                doc_count: 0,
                size_bytes: None,
                last_updated_ts: None,
                status_message: "ok".to_owned(),
            }
        }
    }

    // ── EmbeddingQueue ──

    #[test]
    fn queue_enqueue_and_drain() {
        let queue = EmbeddingQueue::new();

        assert!(queue.enqueue(make_request(1)));
        assert!(queue.enqueue(make_request(2)));
        assert!(queue.enqueue(make_request(3)));

        assert_eq!(queue.len(), 3);
        assert!(!queue.is_empty());

        let batch = queue.drain_batch(2);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].doc_id, 1);
        assert_eq!(batch[1].doc_id, 2);

        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn queue_deduplication() {
        let queue = EmbeddingQueue::new();

        assert!(queue.enqueue(make_request(1)));
        assert!(queue.enqueue(make_request(1))); // Duplicate

        let stats = queue.stats();
        assert_eq!(stats.total_deduped, 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn queue_backpressure() {
        let config = EmbeddingJobConfig {
            backpressure_threshold: 3,
            ..Default::default()
        };
        let queue = EmbeddingQueue::with_config(config);

        assert!(queue.enqueue(make_request(1)));
        assert!(queue.enqueue(make_request(2)));
        assert!(queue.enqueue(make_request(3)));
        assert!(!queue.enqueue(make_request(4))); // Dropped

        let stats = queue.stats();
        assert_eq!(stats.total_dropped, 1);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn queue_retry_priority() {
        let config = EmbeddingJobConfig {
            retry_base_delay_ms: 0,
            ..Default::default()
        };
        let queue = EmbeddingQueue::with_config(config);

        // Enqueue normal requests
        queue.enqueue(make_request(1));
        queue.enqueue(make_request(2));

        // Enqueue retry
        queue.enqueue_retry(make_request(100));

        // Retry should come first
        let batch = queue.drain_batch(10);
        assert_eq!(batch[0].doc_id, 100);
        assert_eq!(batch[0].retries, 1);
    }

    #[test]
    fn queue_retry_backoff_delays_visibility() {
        let config = EmbeddingJobConfig {
            retry_base_delay_ms: 50,
            ..Default::default()
        };
        let queue = EmbeddingQueue::with_config(config);
        queue.enqueue_retry(make_request(1));

        assert!(queue.drain_batch(1).is_empty());
        std::thread::sleep(Duration::from_millis(70));

        let batch = queue.drain_batch(1);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].doc_id, 1);
        assert_eq!(batch[0].retries, 1);
    }

    // ── JobMetrics ──

    #[test]
    fn metrics_record_batch() {
        let metrics = JobMetrics::new();

        let result = BatchResult {
            succeeded: 10,
            retryable: 2,
            failed: 1,
            skipped: 3,
            elapsed: Duration::from_millis(100),
            details: Vec::new(),
        };

        metrics.record_batch(&result);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_succeeded, 10);
        assert_eq!(snapshot.total_retryable, 2);
        assert_eq!(snapshot.total_failed, 1);
        assert_eq!(snapshot.total_skipped, 3);
        assert_eq!(snapshot.total_batches, 1);
    }

    // ── EmbeddingJobRunner ──

    #[test]
    fn runner_process_empty_batch() {
        let config = EmbeddingJobConfig::default();
        let queue = Arc::new(EmbeddingQueue::with_config(config.clone()));
        let embedder = Arc::new(HashEmbedder::new());
        let index = Arc::new(RwLock::new(VectorIndex::new(VectorIndexConfig {
            dimension: 0,
            ..Default::default()
        })));

        let runner = EmbeddingJobRunner::new(config, queue, embedder, index);
        let result = runner.process_batch().unwrap();

        assert_eq!(result.total(), 0);
    }

    #[test]
    fn runner_process_batch_with_hash_embedder() {
        let config = EmbeddingJobConfig {
            batch_size: 10,
            ..Default::default()
        };
        let queue = Arc::new(EmbeddingQueue::with_config(config.clone()));
        let embedder = Arc::new(HashEmbedder::new());
        let index = Arc::new(RwLock::new(VectorIndex::new(VectorIndexConfig {
            dimension: 0,
            ..Default::default()
        })));

        // Enqueue some requests
        queue.enqueue(make_request(1));
        queue.enqueue(make_request(2));

        let runner = EmbeddingJobRunner::new(config, queue, embedder, index);
        let result = runner.process_batch().unwrap();

        // Hash embedder produces hash-only embeddings, which are skipped
        assert_eq!(result.skipped, 2);
        assert_eq!(result.succeeded, 0);
    }

    // ── BatchResult ──

    #[test]
    fn batch_result_total() {
        let result = BatchResult {
            succeeded: 5,
            retryable: 2,
            failed: 1,
            skipped: 3,
            elapsed: Duration::ZERO,
            details: Vec::new(),
        };
        assert_eq!(result.total(), 11);
    }

    // ── JobResult ──

    #[test]
    fn job_result_is_success() {
        let success = JobResult::Success {
            doc_id: 1,
            doc_kind: DocKind::Message,
            model_id: "test".to_owned(),
            content_hash: "hash".to_owned(),
            dimension: 384,
            elapsed: Duration::ZERO,
        };
        assert!(success.is_success());

        let failed = JobResult::Failed {
            doc_id: 1,
            doc_kind: DocKind::Message,
            error: "error".to_owned(),
        };
        assert!(!failed.is_success());
    }

    #[test]
    fn job_result_should_retry() {
        let retryable = JobResult::Retryable {
            doc_id: 1,
            doc_kind: DocKind::Message,
            error: "error".to_owned(),
            retries: 1,
        };
        assert!(retryable.should_retry());

        let failed = JobResult::Failed {
            doc_id: 1,
            doc_kind: DocKind::Message,
            error: "error".to_owned(),
        };
        assert!(!failed.should_retry());
    }

    // ── Config defaults ──

    #[test]
    fn config_defaults() {
        let config = EmbeddingJobConfig::default();
        assert_eq!(config.batch_size, 32);
        assert_eq!(config.max_retries, 3);
        assert!(config.persist_to_db);
    }

    #[test]
    fn refresh_worker_config_defaults() {
        let config = RefreshWorkerConfig::default();
        assert_eq!(config.refresh_interval_ms, 1000);
        assert!(!config.rebuild_on_startup);
    }

    #[test]
    fn refresh_worker_cycle_respects_max_docs_per_cycle() {
        let config = EmbeddingJobConfig {
            batch_size: 16,
            retry_base_delay_ms: 0,
            ..Default::default()
        };
        let queue = Arc::new(EmbeddingQueue::with_config(config.clone()));
        for id in 0..5 {
            assert!(queue.enqueue(make_request(id)));
        }

        let embedder = Arc::new(FixedEmbedder::new(4));
        let index = Arc::new(RwLock::new(VectorIndex::new(VectorIndexConfig {
            dimension: 4,
            ..Default::default()
        })));
        let runner = Arc::new(EmbeddingJobRunner::new(
            config,
            queue.clone(),
            embedder,
            index,
        ));
        let worker = IndexRefreshWorker::new(
            RefreshWorkerConfig {
                max_docs_per_cycle: 3,
                ..Default::default()
            },
            runner,
        );

        let processed = worker.run_cycle();
        assert_eq!(processed, 3);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn refresh_worker_startup_rebuild_uses_lifecycle_hook() {
        let config = EmbeddingJobConfig::default();
        let queue = Arc::new(EmbeddingQueue::with_config(config.clone()));
        let embedder = Arc::new(HashEmbedder::new());
        let index = Arc::new(RwLock::new(VectorIndex::new(VectorIndexConfig {
            dimension: 0,
            ..Default::default()
        })));
        let runner = Arc::new(EmbeddingJobRunner::new(config, queue, embedder, index));
        let lifecycle = Arc::new(MockLifecycle::default());
        let worker = IndexRefreshWorker::new(
            RefreshWorkerConfig {
                rebuild_on_startup: true,
                ..Default::default()
            },
            runner,
        )
        .with_rebuild_lifecycle(lifecycle.clone());

        worker.run_startup_rebuild();
        assert_eq!(lifecycle.rebuild_calls.load(Ordering::Relaxed), 1);
    }
}
