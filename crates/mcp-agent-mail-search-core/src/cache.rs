//! Hybrid search cache and warm worker infrastructure.
//!
//! This module provides query-level caching for Search V3:
//! - [`QueryCacheKey`] - deterministic cache key including mode, filters, and index epoch
//! - [`QueryCache`] - bounded LRU cache with memory controls and eviction telemetry
//! - [`WarmWorker`] - background worker for pre-warming search resources
//! - [`CacheInvalidator`] - epoch-aware cache invalidation on index/model changes

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::query::{SearchFilter, SearchMode};

/// Environment variable for max cache entries.
pub const CACHE_MAX_ENTRIES_ENV: &str = "AM_SEARCH_CACHE_MAX_ENTRIES";
/// Default maximum cache entries.
pub const DEFAULT_CACHE_MAX_ENTRIES: usize = 10_000;

/// Environment variable for cache TTL seconds.
pub const CACHE_TTL_SECONDS_ENV: &str = "AM_SEARCH_CACHE_TTL_SECONDS";
/// Default cache TTL in seconds.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 300;

/// Deterministic cache key for hybrid search queries.
///
/// The key incorporates all factors that affect search results:
/// - Query text (normalized)
/// - Search mode (lexical/semantic/hybrid/auto)
/// - Active filters (sender, project, date range, importance)
/// - Index epoch (invalidated on index updates)
/// - Pagination parameters (offset, limit)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryCacheKey {
    /// Normalized query text (lowercased, trimmed).
    pub query_normalized: String,
    /// Search mode.
    pub mode: SearchMode,
    /// Canonical filter representation for hashing.
    pub filter_hash: u64,
    /// Index epoch at query time.
    pub index_epoch: u64,
    /// Result offset for pagination.
    pub offset: usize,
    /// Result limit.
    pub limit: usize,
}

impl QueryCacheKey {
    /// Create a cache key from query parameters.
    #[must_use]
    pub fn new(
        query: &str,
        mode: SearchMode,
        filter: &SearchFilter,
        index_epoch: u64,
        offset: usize,
        limit: usize,
    ) -> Self {
        Self {
            query_normalized: query.trim().to_lowercase(),
            mode,
            filter_hash: hash_filter(filter),
            index_epoch,
            offset,
            limit,
        }
    }

    /// Create a cache key for "any filter" queries (filter-agnostic).
    #[must_use]
    pub fn without_filter(
        query: &str,
        mode: SearchMode,
        index_epoch: u64,
        offset: usize,
        limit: usize,
    ) -> Self {
        Self {
            query_normalized: query.trim().to_lowercase(),
            mode,
            filter_hash: 0,
            index_epoch,
            offset,
            limit,
        }
    }
}

/// Hash a `SearchFilter` deterministically for cache keying.
fn hash_filter(filter: &SearchFilter) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();

    // Hash each filter component in a deterministic order
    if let Some(ref sender) = filter.sender {
        "sender".hash(&mut hasher);
        sender.hash(&mut hasher);
    }
    if let Some(project_id) = filter.project_id {
        "project_id".hash(&mut hasher);
        project_id.hash(&mut hasher);
    }
    if let Some(ref thread_id) = filter.thread_id {
        "thread_id".hash(&mut hasher);
        thread_id.hash(&mut hasher);
    }
    if let Some(ref date_range) = filter.date_range {
        "date_range".hash(&mut hasher);
        if let Some(start) = date_range.start {
            "start".hash(&mut hasher);
            start.hash(&mut hasher);
        }
        if let Some(end) = date_range.end {
            "end".hash(&mut hasher);
            end.hash(&mut hasher);
        }
    }
    if let Some(ref importance) = filter.importance {
        "importance".hash(&mut hasher);
        format!("{importance:?}").hash(&mut hasher);
    }
    if let Some(ref doc_kind) = filter.doc_kind {
        "doc_kind".hash(&mut hasher);
        format!("{doc_kind:?}").hash(&mut hasher);
    }

    hasher.finish()
}

/// Cached search result entry.
#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    /// Cached value.
    pub value: T,
    /// When the entry was created.
    pub created_at: Instant,
    /// Number of times this entry was accessed.
    pub access_count: u64,
    /// Last access time.
    pub last_accessed: Instant,
}

impl<T> CacheEntry<T> {
    /// Create a new cache entry.
    #[must_use]
    pub fn new(value: T) -> Self {
        let now = Instant::now();
        Self {
            value,
            created_at: now,
            access_count: 1,
            last_accessed: now,
        }
    }

    /// Check if entry is expired.
    #[must_use]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.created_at.elapsed() > ttl
    }

    /// Record an access to this entry.
    pub fn touch(&mut self) {
        self.access_count += 1;
        self.last_accessed = Instant::now();
    }
}

/// Cache eviction metrics.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CacheMetrics {
    /// Total cache hits.
    pub hits: u64,
    /// Total cache misses.
    pub misses: u64,
    /// Total evictions due to capacity.
    pub evictions_capacity: u64,
    /// Total evictions due to TTL expiry.
    pub evictions_ttl: u64,
    /// Total evictions due to epoch invalidation.
    pub evictions_epoch: u64,
    /// Total entries inserted.
    pub inserts: u64,
    /// Current entry count.
    pub current_entries: usize,
}

impl CacheMetrics {
    /// Hit rate as a percentage.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Precision loss is acceptable for metrics
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (self.hits as f64 / total as f64) * 100.0
        }
    }
}

/// Configuration for the query cache.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Maximum number of entries.
    pub max_entries: usize,
    /// Time-to-live for entries.
    pub ttl: Duration,
    /// Enable cache (can be disabled for debugging).
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_CACHE_MAX_ENTRIES,
            ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECONDS),
            enabled: true,
        }
    }
}

impl CacheConfig {
    /// Load config from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let max_entries = std::env::var(CACHE_MAX_ENTRIES_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_CACHE_MAX_ENTRIES);

        let ttl_seconds = std::env::var(CACHE_TTL_SECONDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_CACHE_TTL_SECONDS);

        Self {
            max_entries,
            ttl: Duration::from_secs(ttl_seconds),
            enabled: true,
        }
    }
}

/// Bounded LRU cache for search query results.
///
/// Thread-safe via `RwLock` with interior mutability for metrics.
pub struct QueryCache<T> {
    config: CacheConfig,
    entries: RwLock<HashMap<QueryCacheKey, CacheEntry<T>>>,
    metrics: RwLock<CacheMetrics>,
    current_epoch: AtomicU64,
}

impl<T: Clone> QueryCache<T> {
    /// Create a new query cache with given config.
    #[must_use]
    pub fn new(config: CacheConfig) -> Self {
        Self {
            config,
            entries: RwLock::new(HashMap::new()),
            metrics: RwLock::new(CacheMetrics::default()),
            current_epoch: AtomicU64::new(0),
        }
    }

    /// Create a cache with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Get a cached value if present and not expired.
    #[allow(clippy::significant_drop_tightening)] // Lock ordering is intentional
    pub fn get(&self, key: &QueryCacheKey) -> Option<T> {
        if !self.config.enabled {
            return None;
        }

        // Check epoch first (quick rejection)
        if key.index_epoch != self.current_epoch.load(Ordering::Acquire) {
            let mut metrics = self.metrics.write().ok()?;
            metrics.misses += 1;
            return None;
        }

        let mut entries = self.entries.write().ok()?;
        let Some(entry) = entries.get_mut(key) else {
            // Key not found - miss
            drop(entries); // Release write lock before acquiring metrics lock
            if let Ok(mut metrics) = self.metrics.write() {
                metrics.misses += 1;
            }
            return None;
        };

        // Check TTL
        if entry.is_expired(self.config.ttl) {
            entries.remove(key);
            let mut metrics = self.metrics.write().ok()?;
            metrics.misses += 1;
            metrics.evictions_ttl += 1;
            metrics.current_entries = entries.len();
            return None;
        }

        entry.touch();

        let mut metrics = self.metrics.write().ok()?;
        metrics.hits += 1;

        Some(entry.value.clone())
    }

    /// Insert a value into the cache.
    pub fn put(&self, key: QueryCacheKey, value: T) {
        if !self.config.enabled {
            return;
        }

        // Don't cache if epoch mismatch
        if key.index_epoch != self.current_epoch.load(Ordering::Acquire) {
            return;
        }

        let Ok(mut entries) = self.entries.write() else {
            return;
        };

        // Evict if at capacity
        if entries.len() >= self.config.max_entries && !entries.contains_key(&key) {
            self.evict_lru(&mut entries);
        }

        entries.insert(key, CacheEntry::new(value));

        if let Ok(mut metrics) = self.metrics.write() {
            metrics.inserts += 1;
            metrics.current_entries = entries.len();
        }
    }

    /// Evict the least recently used entry.
    fn evict_lru(&self, entries: &mut HashMap<QueryCacheKey, CacheEntry<T>>) {
        // Find LRU entry
        let lru_key = entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_accessed)
            .map(|(k, _)| k.clone());

        if let Some(key) = lru_key {
            entries.remove(&key);
            if let Ok(mut metrics) = self.metrics.write() {
                metrics.evictions_capacity += 1;
                metrics.current_entries = entries.len();
            }
        }
    }

    /// Invalidate all entries (epoch bump).
    ///
    /// This is called when the index is updated, making all cached results stale.
    pub fn invalidate_all(&self) {
        self.current_epoch.fetch_add(1, Ordering::Release);

        if let Ok(mut entries) = self.entries.write() {
            let count = entries.len();
            entries.clear();

            if let Ok(mut metrics) = self.metrics.write() {
                metrics.evictions_epoch += count as u64;
                metrics.current_entries = 0;
            }
        }
    }

    /// Get the current index epoch.
    #[must_use]
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch.load(Ordering::Acquire)
    }

    /// Bump the epoch (used when index is updated).
    pub fn bump_epoch(&self) -> u64 {
        self.current_epoch.fetch_add(1, Ordering::Release) + 1
    }

    /// Get cache metrics snapshot.
    #[must_use]
    pub fn metrics(&self) -> CacheMetrics {
        self.metrics.read().map(|m| *m).unwrap_or_default()
    }

    /// Prune expired entries (can be called periodically).
    pub fn prune_expired(&self) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };

        let before = entries.len();
        entries.retain(|_, entry| !entry.is_expired(self.config.ttl));
        let removed = before - entries.len();

        if removed > 0 {
            if let Ok(mut metrics) = self.metrics.write() {
                metrics.evictions_ttl += removed as u64;
                metrics.current_entries = entries.len();
            }
        }
    }
}

/// Resource warmup state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarmState {
    /// Resource not yet warmed.
    Cold,
    /// Warmup in progress.
    Warming,
    /// Resource is warm and ready.
    Warm,
    /// Warmup failed.
    Failed,
}

/// Resource type for warmup tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WarmResource {
    /// Lexical index (Tantivy).
    LexicalIndex,
    /// Semantic embedder model.
    SemanticEmbedder,
    /// Vector index (mmap).
    VectorIndex,
    /// Reranker model.
    Reranker,
}

/// Warmup status for a resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmStatus {
    /// Resource type.
    pub resource: WarmResource,
    /// Current state.
    pub state: WarmState,
    /// Time spent warming (if completed).
    pub warm_duration_ms: Option<u64>,
    /// Last warmup attempt.
    pub last_attempt: Option<String>,
    /// Error message if failed.
    pub error: Option<String>,
}

/// Warm worker configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WarmWorkerConfig {
    /// Enable warmup on startup.
    pub warmup_on_startup: bool,
    /// Warmup timeout per resource.
    pub warmup_timeout: Duration,
    /// Retry failed warmups.
    pub retry_on_failure: bool,
    /// Max retry attempts.
    pub max_retries: u32,
}

impl Default for WarmWorkerConfig {
    fn default() -> Self {
        Self {
            warmup_on_startup: true,
            warmup_timeout: Duration::from_secs(30),
            retry_on_failure: true,
            max_retries: 3,
        }
    }
}

/// Warm worker for pre-loading search resources.
///
/// This is a placeholder for the warm worker infrastructure.
/// Actual warmup logic depends on the specific resource implementations.
pub struct WarmWorker {
    config: WarmWorkerConfig,
    status: RwLock<HashMap<WarmResource, WarmStatus>>,
}

impl WarmWorker {
    /// Create a new warm worker.
    #[must_use]
    pub fn new(config: WarmWorkerConfig) -> Self {
        Self {
            config,
            status: RwLock::new(HashMap::new()),
        }
    }

    /// Create with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(WarmWorkerConfig::default())
    }

    /// Get warmup status for a resource.
    #[must_use]
    pub fn get_status(&self, resource: WarmResource) -> Option<WarmStatus> {
        self.status.read().ok()?.get(&resource).cloned()
    }

    /// Get all warmup statuses.
    #[must_use]
    pub fn all_status(&self) -> Vec<WarmStatus> {
        self.status
            .read()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Check if all resources are warm.
    #[must_use]
    pub fn is_fully_warm(&self) -> bool {
        self.status
            .read()
            .is_ok_and(|s| s.values().all(|ws| ws.state == WarmState::Warm))
    }

    /// Record warmup start for a resource.
    pub fn start_warmup(&self, resource: WarmResource) {
        if let Ok(mut status) = self.status.write() {
            status.insert(
                resource,
                WarmStatus {
                    resource,
                    state: WarmState::Warming,
                    warm_duration_ms: None,
                    last_attempt: Some(chrono::Utc::now().to_rfc3339()),
                    error: None,
                },
            );
        }
    }

    /// Record warmup completion.
    #[allow(clippy::cast_possible_truncation)] // Duration in ms won't exceed u64
    pub fn complete_warmup(&self, resource: WarmResource, duration: Duration) {
        if let Ok(mut status) = self.status.write() {
            if let Some(ws) = status.get_mut(&resource) {
                ws.state = WarmState::Warm;
                ws.warm_duration_ms = Some(duration.as_millis() as u64);
                ws.error = None;
            }
        }
    }

    /// Record warmup failure.
    pub fn fail_warmup(&self, resource: WarmResource, error: &str) {
        if let Ok(mut status) = self.status.write() {
            if let Some(ws) = status.get_mut(&resource) {
                ws.state = WarmState::Failed;
                ws.error = Some(error.to_string());
            }
        }
    }

    /// Get the warmup config.
    #[must_use]
    pub const fn config(&self) -> WarmWorkerConfig {
        self.config
    }
}

/// Cache invalidation trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvalidationTrigger {
    /// Index was updated with new documents.
    IndexUpdate,
    /// Index was rebuilt from scratch.
    IndexRebuild,
    /// Embedding model changed.
    ModelSwap,
    /// Manual invalidation requested.
    Manual,
    /// TTL-based expiry.
    TtlExpiry,
}

/// Cache invalidation event for telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvalidationEvent {
    /// What triggered the invalidation.
    pub trigger: InvalidationTrigger,
    /// Timestamp of invalidation.
    pub timestamp: String,
    /// Number of entries invalidated.
    pub entries_invalidated: usize,
    /// New epoch after invalidation.
    pub new_epoch: u64,
}

/// Cache invalidator for coordinating invalidation across caches.
pub struct CacheInvalidator<T> {
    cache: Arc<QueryCache<T>>,
    events: RwLock<Vec<InvalidationEvent>>,
    max_events: usize,
}

impl<T: Clone> CacheInvalidator<T> {
    /// Create a new invalidator for a cache.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // RwLock::new is not const
    pub fn new(cache: Arc<QueryCache<T>>, max_events: usize) -> Self {
        Self {
            cache,
            events: RwLock::new(Vec::new()),
            max_events,
        }
    }

    /// Invalidate cache due to a trigger.
    pub fn invalidate(&self, trigger: InvalidationTrigger) {
        let entries_before = self.cache.metrics().current_entries;
        self.cache.invalidate_all();
        let new_epoch = self.cache.current_epoch();

        let event = InvalidationEvent {
            trigger,
            timestamp: chrono::Utc::now().to_rfc3339(),
            entries_invalidated: entries_before,
            new_epoch,
        };

        if let Ok(mut events) = self.events.write() {
            events.push(event);
            // Keep only recent events
            if events.len() > self.max_events {
                events.remove(0);
            }
        }
    }

    /// Get recent invalidation events.
    #[must_use]
    pub fn recent_events(&self) -> Vec<InvalidationEvent> {
        self.events.read().map(|e| e.clone()).unwrap_or_default()
    }

    /// Get the underlying cache.
    #[must_use]
    pub const fn cache(&self) -> &Arc<QueryCache<T>> {
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_normalization() {
        let filter = SearchFilter::default();
        let key1 = QueryCacheKey::new("  Hello World  ", SearchMode::Hybrid, &filter, 1, 0, 10);
        let key2 = QueryCacheKey::new("hello world", SearchMode::Hybrid, &filter, 1, 0, 10);

        assert_eq!(key1.query_normalized, key2.query_normalized);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_cache_key_mode_differentiation() {
        let filter = SearchFilter::default();
        let key1 = QueryCacheKey::new("test", SearchMode::Lexical, &filter, 1, 0, 10);
        let key2 = QueryCacheKey::new("test", SearchMode::Semantic, &filter, 1, 0, 10);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_epoch_differentiation() {
        let filter = SearchFilter::default();
        let key1 = QueryCacheKey::new("test", SearchMode::Hybrid, &filter, 1, 0, 10);
        let key2 = QueryCacheKey::new("test", SearchMode::Hybrid, &filter, 2, 0, 10);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_put_and_get() {
        let cache: QueryCache<Vec<i64>> = QueryCache::with_defaults();
        let key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 0, 0, 10);

        cache.put(key.clone(), vec![1, 2, 3]);

        let result = cache.get(&key);
        assert_eq!(result, Some(vec![1, 2, 3]));
    }

    #[test]
    fn test_cache_miss_on_epoch_mismatch() {
        let cache: QueryCache<Vec<i64>> = QueryCache::with_defaults();

        // Insert with epoch 0
        let key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 0, 0, 10);
        cache.put(key, vec![1, 2, 3]);

        // Try to get with epoch 1
        let stale_key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 1, 0, 10);
        assert!(cache.get(&stale_key).is_none());
    }

    #[test]
    fn test_cache_invalidate_all() {
        let cache: QueryCache<Vec<i64>> = QueryCache::with_defaults();
        let key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 0, 0, 10);

        cache.put(key.clone(), vec![1, 2, 3]);
        assert!(cache.get(&key).is_some());

        cache.invalidate_all();

        // Entry should be gone and epoch bumped
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.current_epoch(), 1);
    }

    #[test]
    fn test_cache_metrics() {
        let cache: QueryCache<Vec<i64>> = QueryCache::with_defaults();
        let key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 0, 0, 10);

        // Miss
        let _ = cache.get(&key);

        // Insert
        cache.put(key.clone(), vec![1, 2, 3]);

        // Hit
        let _ = cache.get(&key);
        let _ = cache.get(&key);

        let metrics = cache.metrics();
        assert_eq!(metrics.hits, 2);
        assert_eq!(metrics.misses, 1);
        assert_eq!(metrics.inserts, 1);
    }

    #[test]
    fn test_cache_lru_eviction() {
        let config = CacheConfig {
            max_entries: 2,
            ttl: Duration::from_secs(300),
            enabled: true,
        };
        let cache: QueryCache<i64> = QueryCache::new(config);

        let key1 = QueryCacheKey::without_filter("a", SearchMode::Hybrid, 0, 0, 10);
        let key2 = QueryCacheKey::without_filter("b", SearchMode::Hybrid, 0, 0, 10);
        let key3 = QueryCacheKey::without_filter("c", SearchMode::Hybrid, 0, 0, 10);

        cache.put(key1.clone(), 1);
        cache.put(key2.clone(), 2);

        // Access key1 to make it more recently used
        let _ = cache.get(&key1);

        // Insert key3, should evict key2 (LRU)
        cache.put(key3.clone(), 3);

        assert!(cache.get(&key1).is_some());
        assert!(cache.get(&key2).is_none()); // Evicted
        assert!(cache.get(&key3).is_some());
    }

    #[test]
    fn test_warm_worker_status_tracking() {
        let worker = WarmWorker::with_defaults();

        worker.start_warmup(WarmResource::LexicalIndex);
        let status = worker.get_status(WarmResource::LexicalIndex).unwrap();
        assert_eq!(status.state, WarmState::Warming);

        worker.complete_warmup(WarmResource::LexicalIndex, Duration::from_millis(100));
        let status = worker.get_status(WarmResource::LexicalIndex).unwrap();
        assert_eq!(status.state, WarmState::Warm);
        assert_eq!(status.warm_duration_ms, Some(100));
    }

    #[test]
    fn test_warm_worker_failure() {
        let worker = WarmWorker::with_defaults();

        worker.start_warmup(WarmResource::SemanticEmbedder);
        worker.fail_warmup(WarmResource::SemanticEmbedder, "Model not found");

        let status = worker.get_status(WarmResource::SemanticEmbedder).unwrap();
        assert_eq!(status.state, WarmState::Failed);
        assert_eq!(status.error, Some("Model not found".to_string()));
    }

    #[test]
    fn test_invalidator_events() {
        let cache = Arc::new(QueryCache::<i64>::with_defaults());
        let invalidator = CacheInvalidator::new(cache.clone(), 10);

        let key = QueryCacheKey::without_filter("test", SearchMode::Hybrid, 0, 0, 10);
        cache.put(key, 42);

        invalidator.invalidate(InvalidationTrigger::IndexUpdate);

        let events = invalidator.recent_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].trigger, InvalidationTrigger::IndexUpdate);
        assert_eq!(events[0].entries_invalidated, 1);
    }

    #[test]
    fn test_filter_hash_determinism() {
        let filter1 = SearchFilter {
            sender: Some("alice".to_string()),
            project_id: Some(42),
            ..Default::default()
        };
        let filter2 = SearchFilter {
            sender: Some("alice".to_string()),
            project_id: Some(42),
            ..Default::default()
        };

        assert_eq!(hash_filter(&filter1), hash_filter(&filter2));
    }

    #[test]
    fn test_filter_hash_sensitivity() {
        let filter1 = SearchFilter {
            sender: Some("alice".to_string()),
            ..Default::default()
        };
        let filter2 = SearchFilter {
            sender: Some("bob".to_string()),
            ..Default::default()
        };

        assert_ne!(hash_filter(&filter1), hash_filter(&filter2));
    }
}
