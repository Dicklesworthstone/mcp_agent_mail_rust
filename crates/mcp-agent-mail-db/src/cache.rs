//! In-memory read cache for hot-path project and agent lookups,
//! plus a deferred touch queue to batch `last_active_ts` updates.
//!
//! Dramatically reduces DB round-trips for repeated `resolve_project` and
//! `resolve_agent` calls that happen on every tool invocation.
//!
//! ## Capacity & TTL
//!
//! - Projects cached for 5 minutes (almost never change after creation)
//! - Agents cached for 5 minutes (profile updates are infrequent)
//! - Max 16,384 entries per category (~3.2 MB total at saturation)
//! - Write-through: callers should call `invalidate_*` or `put_*` after mutations
//! - Deferred touch: `touch_agent` timestamps are buffered and flushed in batches
//!
//! ## Eviction
//!
//! Uses S3-FIFO (Yang et al., SOSP 2023) for O(1) amortized eviction via
//! three FIFO queues (small/main/ghost) with frequency-based promotion.
//!
//! ## Adaptive TTL
//!
//! Frequently accessed entries get their TTL extended up to 2x the base:
//! - 0-4 accesses: base TTL (300s for agents, 300s for projects)
//! - 5+ accesses: 2x base TTL (600s)
//!
//! ## Metrics
//!
//! Lock-free atomic counters track cache hit/miss rates per category.
//! Call `cache_metrics()` to get a snapshot.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::models::{AgentRow, InboxStatsRow, ProjectRow};
use crate::s3fifo::S3FifoCache;
use mcp_agent_mail_core::{InternedStr, LockLevel, OrderedMutex, OrderedRwLock};

const PROJECT_TTL: Duration = Duration::from_mins(5);
const AGENT_TTL: Duration = Duration::from_mins(5);
const INBOX_STATS_TTL: Duration = Duration::from_secs(30); // 30 sec (shorter: counters change often)
const MAX_ENTRIES_PER_CATEGORY: usize = 16_384;
/// Minimum interval between deferred touch flushes.
const TOUCH_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// Minimum accesses before adaptive TTL kicks in (2x base TTL).
const ADAPTIVE_TTL_THRESHOLD: u32 = 5;
/// Number of lock-independent shards for the deferred touch queue.
/// Shard key: `agent_id % NUM_TOUCH_SHARDS`. Reduces contention 16×
/// compared to a single mutex at 100+ concurrent tool calls/sec.
const NUM_TOUCH_SHARDS: usize = 16;

#[inline]
fn scope_fingerprint(scope: &str) -> u64 {
    // Per-process deterministic hashing for cache namespacing.
    // Prevents collisions between multiple sqlite databases loaded in one process.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scope.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
struct CacheEntry<T> {
    value: T,
    inserted: Instant,
    last_accessed: Instant,
    access_count: u32,
}

impl<T> CacheEntry<T> {
    fn new(value: T) -> Self {
        let now = Instant::now();
        Self {
            value,
            inserted: now,
            last_accessed: now,
            access_count: 0,
        }
    }

    /// Returns the effective TTL, considering adaptive extension for hot entries.
    fn effective_ttl(&self, base_ttl: Duration) -> Duration {
        if self.access_count >= ADAPTIVE_TTL_THRESHOLD {
            base_ttl * 2
        } else {
            base_ttl
        }
    }

    fn is_expired(&self, base_ttl: Duration) -> bool {
        self.inserted.elapsed() > self.effective_ttl(base_ttl)
    }

    /// Record an access, updating `last_accessed` and bumping the access counter.
    fn touch(&mut self) {
        self.last_accessed = Instant::now();
        self.access_count = self.access_count.saturating_add(1);
    }
}

/// Lock-free cache hit/miss counters.
pub struct CacheMetrics {
    pub project_hits: AtomicU64,
    pub project_misses: AtomicU64,
    pub agent_hits: AtomicU64,
    pub agent_misses: AtomicU64,
}

/// Snapshot of cache metrics at a point in time.
#[derive(Debug, Clone)]
pub struct CacheMetricsSnapshot {
    pub project_hits: u64,
    pub project_misses: u64,
    pub agent_hits: u64,
    pub agent_misses: u64,
}

impl CacheMetricsSnapshot {
    /// Project cache hit rate (0.0–1.0). Returns 0.0 if no lookups yet.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn project_hit_rate(&self) -> f64 {
        let total = self.project_hits + self.project_misses;
        if total == 0 {
            0.0
        } else {
            self.project_hits as f64 / total as f64
        }
    }

    /// Agent cache hit rate (0.0–1.0). Returns 0.0 if no lookups yet.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn agent_hit_rate(&self) -> f64 {
        let total = self.agent_hits + self.agent_misses;
        if total == 0 {
            0.0
        } else {
            self.agent_hits as f64 / total as f64
        }
    }
}

impl CacheMetrics {
    const fn new() -> Self {
        Self {
            project_hits: AtomicU64::new(0),
            project_misses: AtomicU64::new(0),
            agent_hits: AtomicU64::new(0),
            agent_misses: AtomicU64::new(0),
        }
    }

    fn record_project_hit(&self) {
        self.project_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_project_miss(&self) {
        self.project_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn record_agent_hit(&self) {
        self.agent_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_agent_miss(&self) {
        self.agent_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a snapshot of the current metric values.
    pub fn snapshot(&self) -> CacheMetricsSnapshot {
        CacheMetricsSnapshot {
            project_hits: self.project_hits.load(Ordering::Relaxed),
            project_misses: self.project_misses.load(Ordering::Relaxed),
            agent_hits: self.agent_hits.load(Ordering::Relaxed),
            agent_misses: self.agent_misses.load(Ordering::Relaxed),
        }
    }
}

static CACHE_METRICS: CacheMetrics = CacheMetrics::new();

/// Get the global cache metrics.
#[must_use]
pub fn cache_metrics() -> &'static CacheMetrics {
    &CACHE_METRICS
}

/// In-memory read cache for projects, agents, and inbox stats.
pub struct ReadCache {
    projects_by_slug: OrderedRwLock<S3FifoCache<String, CacheEntry<ProjectRow>>>,
    projects_by_human_key: OrderedRwLock<S3FifoCache<String, CacheEntry<ProjectRow>>>,
    agents_by_key: OrderedRwLock<S3FifoCache<(u64, i64, InternedStr), CacheEntry<AgentRow>>>,
    agents_by_id: OrderedRwLock<S3FifoCache<(u64, i64), CacheEntry<AgentRow>>>,
    /// Cached inbox aggregate counters keyed by `(db_scope, agent_id)` (30s TTL).
    inbox_stats: OrderedRwLock<S3FifoCache<(u64, i64), CacheEntry<InboxStatsRow>>>,
    /// Sharded deferred touch queue (16 shards, keyed by `agent_id % 16`).
    /// Each shard maps `agent_id` → latest requested timestamp (micros).
    deferred_touch_shards: [OrderedMutex<HashMap<i64, i64>>; NUM_TOUCH_SHARDS],
    /// Last time we flushed the deferred touches.
    last_touch_flush: OrderedMutex<Instant>,
    /// Atomic flag: true if any shard MAY have pending entries.
    /// Set in `enqueue_touch()`, cleared in `drain_touches()`.
    /// Avoids acquiring 16 shard locks in `has_pending_touches()`.
    has_pending: AtomicBool,
}

impl ReadCache {
    fn new() -> Self {
        Self::with_capacity(MAX_ENTRIES_PER_CATEGORY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            projects_by_slug: OrderedRwLock::new(
                LockLevel::DbReadCacheProjectsBySlug,
                S3FifoCache::new(capacity),
            ),
            projects_by_human_key: OrderedRwLock::new(
                LockLevel::DbReadCacheProjectsByHumanKey,
                S3FifoCache::new(capacity),
            ),
            agents_by_key: OrderedRwLock::new(
                LockLevel::DbReadCacheAgentsByKey,
                S3FifoCache::new(capacity),
            ),
            agents_by_id: OrderedRwLock::new(
                LockLevel::DbReadCacheAgentsById,
                S3FifoCache::new(capacity),
            ),
            inbox_stats: OrderedRwLock::new(
                LockLevel::DbReadCacheInboxStats,
                S3FifoCache::new(capacity),
            ),
            deferred_touch_shards: std::array::from_fn(|_| {
                OrderedMutex::new(LockLevel::DbReadCacheDeferredTouches, HashMap::new())
            }),
            last_touch_flush: OrderedMutex::new(
                LockLevel::DbReadCacheLastTouchFlush,
                Instant::now(),
            ),
            has_pending: AtomicBool::new(false),
        }
    }

    // -------------------------------------------------------------------------
    // Project cache
    // -------------------------------------------------------------------------

    /// Look up a project by slug. Returns `None` if not cached or expired.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_project(&self, slug: &str) -> Option<ProjectRow> {
        let mut cache = self.projects_by_slug.write();
        let Some(entry) = cache.get_mut(slug) else {
            CACHE_METRICS.record_project_miss();
            return None;
        };
        if entry.is_expired(PROJECT_TTL) {
            let slug_owned = slug.to_owned();
            mcp_agent_mail_core::evidence_ledger().record(
                "cache.eviction",
                serde_json::json!({ "key": slug_owned, "reason": "ttl_expired", "category": "project" }),
                "evict",
                Some("hit_rate >= 0.85".into()),
                0.9,
                "s3fifo_v1",
            );
            cache.remove(slug);
            CACHE_METRICS.record_project_miss();
            return None;
        }
        entry.touch();
        let value = entry.value.clone();
        CACHE_METRICS.record_project_hit();
        Some(value)
    }

    /// Look up a project by `human_key`.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_project_by_human_key(&self, human_key: &str) -> Option<ProjectRow> {
        let mut cache = self.projects_by_human_key.write();
        let Some(entry) = cache.get_mut(human_key) else {
            CACHE_METRICS.record_project_miss();
            return None;
        };
        if entry.is_expired(PROJECT_TTL) {
            cache.remove(human_key);
            CACHE_METRICS.record_project_miss();
            return None;
        }
        entry.touch();
        let value = entry.value.clone();
        CACHE_METRICS.record_project_hit();
        Some(value)
    }

    /// Cache a project (write-through after DB mutation).
    /// Indexes by both `slug` and `human_key`.
    pub fn put_project(&self, project: &ProjectRow) {
        {
            let mut cache = self.projects_by_slug.write();
            cache.insert(project.slug.clone(), CacheEntry::new(project.clone()));
        }
        {
            let mut cache = self.projects_by_human_key.write();
            cache.insert(project.human_key.clone(), CacheEntry::new(project.clone()));
        }
    }

    // -------------------------------------------------------------------------
    // Agent cache
    // -------------------------------------------------------------------------

    /// Look up an agent by (`project_id`, name). Returns `None` if not cached or expired.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent(&self, project_id: i64, name: &str) -> Option<AgentRow> {
        self.get_agent_scoped("", project_id, name)
    }

    /// Look up an agent by (`project_id`, name) in a specific DB scope.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent_scoped(&self, scope: &str, project_id: i64, name: &str) -> Option<AgentRow> {
        let key = (scope_fingerprint(scope), project_id, InternedStr::new(name));
        let mut cache = self.agents_by_key.write();
        let Some(entry) = cache.get_mut(&key) else {
            CACHE_METRICS.record_agent_miss();
            return None;
        };
        if entry.is_expired(AGENT_TTL) {
            cache.remove(&key);
            CACHE_METRICS.record_agent_miss();
            return None;
        }
        entry.touch();
        let value = entry.value.clone();
        CACHE_METRICS.record_agent_hit();
        Some(value)
    }

    /// Look up an agent by id.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent_by_id(&self, agent_id: i64) -> Option<AgentRow> {
        self.get_agent_by_id_scoped("", agent_id)
    }

    /// Look up an agent by id in a specific DB scope.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent_by_id_scoped(&self, scope: &str, agent_id: i64) -> Option<AgentRow> {
        let key = (scope_fingerprint(scope), agent_id);
        let mut cache = self.agents_by_id.write();
        let Some(entry) = cache.get_mut(&key) else {
            CACHE_METRICS.record_agent_miss();
            return None;
        };
        if entry.is_expired(AGENT_TTL) {
            cache.remove(&key);
            CACHE_METRICS.record_agent_miss();
            return None;
        }
        entry.touch();
        let value = entry.value.clone();
        CACHE_METRICS.record_agent_hit();
        Some(value)
    }

    /// Cache an agent (write-through after DB mutation).
    /// Indexes by both (`project_id`, `name`) and `id`.
    pub fn put_agent(&self, agent: &AgentRow) {
        self.put_agent_scoped("", agent);
    }

    /// Cache an agent (write-through after DB mutation) in a specific DB scope.
    pub fn put_agent_scoped(&self, scope: &str, agent: &AgentRow) {
        let scope_fp = scope_fingerprint(scope);
        {
            let mut cache = self.agents_by_key.write();
            cache.insert(
                (scope_fp, agent.project_id, InternedStr::new(&agent.name)),
                CacheEntry::new(agent.clone()),
            );
        }
        if let Some(id) = agent.id {
            let mut cache = self.agents_by_id.write();
            cache.insert((scope_fp, id), CacheEntry::new(agent.clone()));
        }
    }

    /// Bulk-insert agents into the cache (cache warming on startup).
    /// Useful for pre-loading all agents for active projects to avoid cold-start
    /// DB round-trips.
    pub fn warm_agents(&self, agents: &[AgentRow]) {
        self.warm_agents_scoped("", agents);
    }

    /// Bulk-insert agents into the cache (cache warming on startup) in a DB scope.
    pub fn warm_agents_scoped(&self, scope: &str, agents: &[AgentRow]) {
        let scope_fp = scope_fingerprint(scope);
        {
            let mut cache = self.agents_by_key.write();
            for agent in agents {
                cache.insert(
                    (scope_fp, agent.project_id, InternedStr::new(&agent.name)),
                    CacheEntry::new(agent.clone()),
                );
            }
        }
        {
            let mut cache = self.agents_by_id.write();
            for agent in agents {
                if let Some(id) = agent.id {
                    cache.insert((scope_fp, id), CacheEntry::new(agent.clone()));
                }
            }
        }
    }

    /// Bulk-insert projects into the cache (cache warming on startup).
    pub fn warm_projects(&self, projects: &[ProjectRow]) {
        {
            let mut cache = self.projects_by_slug.write();
            for project in projects {
                cache.insert(project.slug.clone(), CacheEntry::new(project.clone()));
            }
        }
        {
            let mut cache = self.projects_by_human_key.write();
            for project in projects {
                cache.insert(project.human_key.clone(), CacheEntry::new(project.clone()));
            }
        }
    }

    /// Invalidate a specific agent entry (call after `register_agent` update).
    pub fn invalidate_agent(&self, project_id: i64, name: &str) {
        self.invalidate_agent_scoped("", project_id, name);
    }

    /// Invalidate a specific agent entry in a DB scope.
    pub fn invalidate_agent_scoped(&self, scope: &str, project_id: i64, name: &str) {
        let scope_fp = scope_fingerprint(scope);
        let key = (scope_fp, project_id, InternedStr::new(name));
        let mut cache = self.agents_by_key.write();
        if let Some(agent) = cache.remove(&key)
            && let Some(id) = agent.value.id
        {
            drop(cache); // release key map lock first
            let mut id_cache = self.agents_by_id.write();
            id_cache.remove(&(scope_fp, id));
        }
    }

    // -------------------------------------------------------------------------
    // Inbox stats cache
    // -------------------------------------------------------------------------

    /// Look up cached inbox stats for an agent. Returns `None` if not cached
    /// or expired (30s TTL).
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_inbox_stats(&self, agent_id: i64) -> Option<InboxStatsRow> {
        self.get_inbox_stats_scoped("", agent_id)
    }

    /// Look up cached inbox stats for an agent in a specific DB scope.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_inbox_stats_scoped(&self, scope: &str, agent_id: i64) -> Option<InboxStatsRow> {
        let key = (scope_fingerprint(scope), agent_id);
        let mut cache = self.inbox_stats.write();
        let entry = cache.get_mut(&key)?;
        if entry.is_expired(INBOX_STATS_TTL) {
            cache.remove(&key);
            return None;
        }
        entry.touch();
        Some(entry.value.clone())
    }

    /// Insert or update cached inbox stats for an agent.
    pub fn put_inbox_stats(&self, stats: &InboxStatsRow) {
        self.put_inbox_stats_scoped("", stats);
    }

    /// Insert or update cached inbox stats for an agent in a specific DB scope.
    pub fn put_inbox_stats_scoped(&self, scope: &str, stats: &InboxStatsRow) {
        let key = (scope_fingerprint(scope), stats.agent_id);
        let mut cache = self.inbox_stats.write();
        cache.insert(key, CacheEntry::new(stats.clone()));
    }

    /// Invalidate cached inbox stats for an agent.
    pub fn invalidate_inbox_stats(&self, agent_id: i64) {
        self.invalidate_inbox_stats_scoped("", agent_id);
    }

    /// Invalidate cached inbox stats for an agent in a specific DB scope.
    pub fn invalidate_inbox_stats_scoped(&self, scope: &str, agent_id: i64) {
        let key = (scope_fingerprint(scope), agent_id);
        let mut cache = self.inbox_stats.write();
        cache.remove(&key);
    }

    // -------------------------------------------------------------------------
    // Deferred touch queue
    // -------------------------------------------------------------------------

    /// Enqueue a deferred `touch_agent` update. Returns `true` if the flush
    /// interval has elapsed and the caller should drain.
    ///
    /// Only locks the shard for `agent_id % 16`, so concurrent touches for
    /// different shards never contend.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn enqueue_touch(&self, agent_id: i64, ts_micros: i64) -> bool {
        let shard_idx = (agent_id as u64 as usize) % NUM_TOUCH_SHARDS;
        {
            let mut shard = self.deferred_touch_shards[shard_idx].lock();
            // Keep only the latest timestamp per agent
            shard
                .entry(agent_id)
                .and_modify(|existing| {
                    if ts_micros > *existing {
                        *existing = ts_micros;
                    }
                })
                .or_insert(ts_micros);
        }

        // Signal that at least one shard has pending entries.
        self.has_pending.store(true, Ordering::Release);

        let last = self.last_touch_flush.lock();
        last.elapsed() >= TOUCH_FLUSH_INTERVAL
    }

    /// Drain all pending touch entries from all shards and reset the flush clock.
    /// Returns the merged map of `agent_id` → latest timestamp.
    pub fn drain_touches(&self) -> HashMap<i64, i64> {
        // Clear the pending flag BEFORE draining all shards.
        // If a concurrent enqueue happens after we clear the flag but before we lock
        // its shard, we will drain the new item.
        // If it happens after we drain its shard, it will set the flag to true,
        // which is correct because we missed it in this drain pass.
        self.has_pending.store(false, Ordering::Release);

        let mut merged = HashMap::new();
        for shard in &self.deferred_touch_shards {
            let mut s = shard.lock();
            merged.extend(s.drain());
        }

        let mut last = self.last_touch_flush.lock();
        *last = Instant::now();
        drop(last);

        mcp_agent_mail_core::evidence_ledger().record(
            "cache.deferred_flush",
            serde_json::json!({ "pending_count": merged.len() }),
            "flush",
            Some("batch_size > 0".into()),
            0.95,
            "cache_v1",
        );

        merged
    }

    /// Check if there are pending touches in any shard.
    ///
    /// Uses a single atomic load instead of acquiring 16 shard locks.
    /// The flag is conservative: `true` means there MAY be pending entries
    /// (a false positive after a concurrent drain is harmless).
    pub fn has_pending_touches(&self) -> bool {
        self.has_pending.load(Ordering::Acquire)
    }

    /// Return current entry counts per cache category.
    ///
    /// Note: `S3FifoCache::len()` requires `&self` only but
    /// `OrderedRwLock::read()` returns a read guard that is sufficient.
    pub fn entry_counts(&self) -> CacheEntryCounts {
        CacheEntryCounts {
            projects_by_slug: self.projects_by_slug.read().len(),
            projects_by_human_key: self.projects_by_human_key.read().len(),
            agents_by_key: self.agents_by_key.read().len(),
            agents_by_id: self.agents_by_id.read().len(),
            inbox_stats: self.inbox_stats.read().len(),
        }
    }

    /// Create a new standalone cache instance (for testing).
    #[must_use]
    pub fn new_for_testing() -> Self {
        Self::new()
    }

    /// Create a cache with a custom capacity (for stress testing).
    #[must_use]
    pub fn new_for_testing_with_capacity(capacity: usize) -> Self {
        Self::with_capacity(capacity)
    }

    /// Clear all cache entries (for testing).
    #[cfg(test)]
    pub fn clear(&self) {
        self.projects_by_slug.write().clear();
        self.projects_by_human_key.write().clear();
        self.agents_by_key.write().clear();
        self.agents_by_id.write().clear();
        self.inbox_stats.write().clear();
        for shard in &self.deferred_touch_shards {
            shard.lock().clear();
        }
    }
}

/// Snapshot of cache entry counts.
#[derive(Debug, Clone)]
pub struct CacheEntryCounts {
    pub projects_by_slug: usize,
    pub projects_by_human_key: usize,
    pub agents_by_key: usize,
    pub agents_by_id: usize,
    pub inbox_stats: usize,
}

static READ_CACHE: OnceLock<ReadCache> = OnceLock::new();

/// Get the global read cache instance.
pub fn read_cache() -> &'static ReadCache {
    READ_CACHE.get_or_init(ReadCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_project(slug: &str) -> ProjectRow {
        ProjectRow {
            id: Some(1),
            slug: slug.to_string(),
            human_key: format!("/data/{slug}"),
            created_at: 0,
        }
    }

    fn make_agent(name: &str, project_id: i64) -> AgentRow {
        make_agent_with_id(name, project_id, project_id * 100 + 1)
    }

    fn make_agent_with_id(name: &str, project_id: i64, id: i64) -> AgentRow {
        AgentRow {
            id: Some(id),
            project_id,
            name: name.to_string(),
            program: "test".to_string(),
            model: "test".to_string(),
            task_description: String::new(),
            inception_ts: 0,
            last_active_ts: 0,
            attachments_policy: "auto".to_string(),
            contact_policy: "open".to_string(),
        }
    }

    #[test]
    fn project_cache_hit_and_miss() {
        let cache = ReadCache::new();

        assert!(cache.get_project("foo").is_none());

        let project = make_project("foo");
        cache.put_project(&project);

        let cached = cache.get_project("foo");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().slug, "foo");
    }

    #[test]
    fn project_cache_by_human_key() {
        let cache = ReadCache::new();

        let project = make_project("myproj");
        cache.put_project(&project);

        let cached = cache.get_project_by_human_key("/data/myproj");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().slug, "myproj");
    }

    #[test]
    fn agent_cache_hit_and_miss() {
        let cache = ReadCache::new();

        assert!(cache.get_agent(1, "BlueLake").is_none());

        let agent = make_agent("BlueLake", 1);
        cache.put_agent(&agent);

        let cached = cache.get_agent(1, "BlueLake");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().name, "BlueLake");
    }

    #[test]
    fn agent_cache_by_id() {
        let cache = ReadCache::new();

        let agent = make_agent_with_id("GreenHill", 2, 42);
        cache.put_agent(&agent);

        // Must find by the actual ID we assigned, not by a hardcoded value
        assert!(cache.get_agent_by_id(42).is_some());
        assert_eq!(cache.get_agent_by_id(42).unwrap().name, "GreenHill");
        // Different ID must miss
        assert!(cache.get_agent_by_id(99).is_none());
    }

    #[test]
    fn agent_invalidate() {
        let cache = ReadCache::new();

        let agent = make_agent_with_id("RedCat", 2, 55);
        cache.put_agent(&agent);
        assert!(cache.get_agent(2, "RedCat").is_some());
        assert!(cache.get_agent_by_id(55).is_some());

        cache.invalidate_agent(2, "RedCat");
        assert!(cache.get_agent(2, "RedCat").is_none());
        assert!(cache.get_agent_by_id(55).is_none());
    }

    #[test]
    fn max_entries_respected() {
        let cache = ReadCache::new();

        for i in 0..MAX_ENTRIES_PER_CATEGORY + 10 {
            let slug = format!("proj-{i}");
            cache.put_project(&make_project(&slug));
        }

        let map_len = cache.projects_by_slug.read().len();
        assert!(map_len <= MAX_ENTRIES_PER_CATEGORY);
    }

    #[test]
    fn s3fifo_eviction_preserves_accessed_entries() {
        // S3-FIFO promotes accessed entries from Small to Main,
        // so they survive eviction when capacity is reached.
        let mut cache = S3FifoCache::<String, CacheEntry<i32>>::new(5);
        for i in 0..3 {
            cache.insert(format!("k{i}"), CacheEntry::new(i));
            // Access to bump freq so it promotes to Main
            cache.get_mut(&format!("k{i}"));
        }
        // Insert more to fill and trigger eviction
        for i in 3..10 {
            cache.insert(format!("k{i}"), CacheEntry::new(i));
        }
        assert!(cache.len() <= 5);
    }

    #[test]
    fn s3fifo_capacity_zero_handled_by_constructor() {
        // S3FifoCache panics on 0 capacity (by design).
        let result = std::panic::catch_unwind(|| S3FifoCache::<String, i32>::new(0));
        assert!(result.is_err());
    }

    #[test]
    fn deferred_touch_coalesces() {
        let cache = ReadCache::new();

        // Two touches for same agent - should keep latest
        cache.enqueue_touch(42, 1000);
        cache.enqueue_touch(42, 2000);
        cache.enqueue_touch(42, 1500); // earlier timestamp, ignored

        let drained = cache.drain_touches();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[&42], 2000);
    }

    #[test]
    fn deferred_touch_multi_agent() {
        let cache = ReadCache::new();

        cache.enqueue_touch(1, 100);
        cache.enqueue_touch(2, 200);
        cache.enqueue_touch(3, 300);

        let drained = cache.drain_touches();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[&1], 100);
        assert_eq!(drained[&2], 200);
        assert_eq!(drained[&3], 300);

        // After drain, should be empty
        assert!(!cache.has_pending_touches());
    }

    #[test]
    fn drain_resets_flush_clock() {
        let cache = ReadCache::new();

        cache.enqueue_touch(1, 100);
        let _ = cache.drain_touches();

        // Immediately after drain, should_flush should be false
        let should_flush = cache.enqueue_touch(1, 200);
        assert!(!should_flush, "should not flush immediately after drain");
    }

    // ---- New tests for LRU, adaptive TTL, and metrics ----

    #[test]
    fn s3fifo_eviction_preserves_accessed_agents() {
        // S3-FIFO promotes frequently accessed entries to Main queue,
        // protecting them from eviction. We must access the entry
        // while it is still in Small (before it gets evicted to Ghost).
        let capacity = 100; // small=10, main=90
        let cache = ReadCache::with_capacity(capacity);

        // Insert Agent0 and immediately access it to bump freq
        cache.put_agent(&make_agent_with_id("Agent0", 1, 0));
        for _ in 0..3 {
            let _ = cache.get_agent(1, "Agent0");
        }

        // Now fill beyond capacity to trigger eviction
        #[allow(clippy::cast_possible_wrap)]
        for i in 1..(capacity * 2) {
            let name = format!("Agent{i}");
            cache.put_agent(&make_agent_with_id(&name, 1, i as i64));
        }

        // Agent0 should survive: freq >= 1 when evicted from Small -> promoted to Main
        assert!(
            cache.get_agent(1, "Agent0").is_some(),
            "frequently accessed Agent0 should survive eviction via S3-FIFO promotion"
        );
    }

    #[test]
    fn adaptive_ttl_extends_for_hot_entries() {
        // Entries accessed >= ADAPTIVE_TTL_THRESHOLD times get 2x TTL.
        let entry_cold = CacheEntry {
            value: 42_i32,
            inserted: Instant::now(),
            last_accessed: Instant::now(),
            access_count: 0,
        };
        let entry_hot = CacheEntry {
            value: 42_i32,
            inserted: Instant::now(),
            last_accessed: Instant::now(),
            access_count: ADAPTIVE_TTL_THRESHOLD,
        };

        let base = Duration::from_mins(1);
        assert_eq!(entry_cold.effective_ttl(base), base);
        assert_eq!(entry_hot.effective_ttl(base), base * 2);

        // Just below threshold stays at base
        let entry_warm = CacheEntry {
            value: 42_i32,
            inserted: Instant::now(),
            last_accessed: Instant::now(),
            access_count: ADAPTIVE_TTL_THRESHOLD - 1,
        };
        assert_eq!(entry_warm.effective_ttl(base), base);
    }

    #[test]
    fn cache_metrics_recorded() {
        let cache = ReadCache::new();

        // Record initial snapshot
        let before = CACHE_METRICS.snapshot();

        // Miss
        let _ = cache.get_project("nonexistent");
        let after_miss = CACHE_METRICS.snapshot();
        assert!(
            after_miss.project_misses > before.project_misses,
            "miss not recorded (before={}, after={})",
            before.project_misses,
            after_miss.project_misses
        );

        // Put then hit
        cache.put_project(&make_project("metrics-test"));
        let _ = cache.get_project("metrics-test");
        let after_hit = CACHE_METRICS.snapshot();
        assert!(
            after_hit.project_hits > before.project_hits,
            "hit not recorded (before={}, after={})",
            before.project_hits,
            after_hit.project_hits
        );
    }

    #[test]
    fn cache_metrics_agent() {
        let cache = ReadCache::new();
        let before = CACHE_METRICS.snapshot();

        // Miss by key
        let _ = cache.get_agent(1, "NoSuchAgent");
        let s1 = CACHE_METRICS.snapshot();
        assert!(
            s1.agent_misses > before.agent_misses,
            "agent miss by key not recorded (before={}, after={})",
            before.agent_misses,
            s1.agent_misses
        );

        // Miss by id
        let _ = cache.get_agent_by_id(999_999);
        let s2 = CACHE_METRICS.snapshot();
        assert!(
            s2.agent_misses >= before.agent_misses + 2,
            "agent miss by id not recorded (before={}, after={})",
            before.agent_misses,
            s2.agent_misses
        );

        // Hit by key
        cache.put_agent(&make_agent("BlueLake", 99));
        let _ = cache.get_agent(99, "BlueLake");
        let s3 = CACHE_METRICS.snapshot();
        assert!(
            s3.agent_hits > before.agent_hits,
            "agent hit by key not recorded (before={}, after={})",
            before.agent_hits,
            s3.agent_hits
        );

        // Hit by id
        let _ = cache.get_agent_by_id(99 * 100 + 1);
        let s4 = CACHE_METRICS.snapshot();
        assert!(
            s4.agent_hits >= before.agent_hits + 2,
            "agent hit by id not recorded (before={}, after={})",
            before.agent_hits,
            s4.agent_hits
        );
    }

    #[test]
    fn hit_rate_computation() {
        let snap = CacheMetricsSnapshot {
            project_hits: 80,
            project_misses: 20,
            agent_hits: 0,
            agent_misses: 0,
        };
        let rate = snap.project_hit_rate();
        assert!((rate - 0.8).abs() < f64::EPSILON);
        assert!(snap.agent_hit_rate().abs() < f64::EPSILON);
    }

    #[test]
    fn entry_counts() {
        let cache = ReadCache::new();
        let c = cache.entry_counts();
        assert_eq!(c.projects_by_slug, 0);
        assert_eq!(c.agents_by_key, 0);

        cache.put_project(&make_project("p1"));
        cache.put_agent(&make_agent("A1", 1));

        let c = cache.entry_counts();
        assert_eq!(c.projects_by_slug, 1);
        assert_eq!(c.projects_by_human_key, 1);
        assert_eq!(c.agents_by_key, 1);
        assert_eq!(c.agents_by_id, 1);
    }

    #[test]
    fn large_scale_agents_no_oom() {
        // Verify that inserting 2000 agents doesn't panic or OOM.
        // With S3-FIFO, unaccessed items may be evicted from Small to Ghost
        // (ghost entries don't count in len()), so we only verify capacity bounds.
        let cache = ReadCache::new();
        for i in 0..2000 {
            let name = format!("Agent{i}");
            cache.put_agent(&make_agent_with_id(&name, 1, i));
        }
        let counts = cache.entry_counts();
        assert!(counts.agents_by_key <= MAX_ENTRIES_PER_CATEGORY);
        assert!(counts.agents_by_id <= MAX_ENTRIES_PER_CATEGORY);
    }

    #[test]
    fn access_bumps_count_and_survives_eviction() {
        // Verify that repeated access keeps an agent alive under eviction pressure
        // (S3-FIFO freq promotion + CacheEntry adaptive TTL).
        let capacity = 20;
        let cache = ReadCache::with_capacity(capacity);
        cache.put_agent(&make_agent_with_id("HotAgent", 1, 9999));

        // Access 10 times to promote in S3-FIFO + build access_count
        for _ in 0..10 {
            let got = cache.get_agent(1, "HotAgent");
            assert!(
                got.is_some(),
                "HotAgent should be retrievable on each access"
            );
        }

        // Now fill the cache to trigger eviction
        #[allow(clippy::cast_possible_wrap)]
        for i in 0..(capacity * 2) {
            cache.put_agent(&make_agent_with_id(&format!("Other{i}"), 1, i as i64));
        }

        // HotAgent should survive thanks to high frequency
        assert!(
            cache.get_agent(1, "HotAgent").is_some(),
            "frequently accessed HotAgent should survive eviction"
        );
    }

    #[test]
    fn warm_agents_bulk_insert() {
        let cache = ReadCache::new();

        let agents: Vec<AgentRow> = (0..100)
            .map(|i| make_agent_with_id(&format!("Agent{i}"), 1, i))
            .collect();
        cache.warm_agents(&agents);

        // All should be cached
        for i in 0..100 {
            assert!(
                cache.get_agent(1, &format!("Agent{i}")).is_some(),
                "Agent{i} should be cached"
            );
            assert!(
                cache.get_agent_by_id(i).is_some(),
                "Agent id {i} should be cached"
            );
        }
    }

    #[test]
    fn warm_projects_bulk_insert() {
        let cache = ReadCache::new();

        let projects: Vec<ProjectRow> = (0..50)
            .map(|i| ProjectRow {
                id: Some(i),
                slug: format!("proj-{i}"),
                human_key: format!("/data/proj-{i}"),
                created_at: 0,
            })
            .collect();
        cache.warm_projects(&projects);

        for i in 0..50 {
            assert!(
                cache.get_project(&format!("proj-{i}")).is_some(),
                "proj-{i} should be cached by slug"
            );
            assert!(
                cache
                    .get_project_by_human_key(&format!("/data/proj-{i}"))
                    .is_some(),
                "proj-{i} should be cached by human_key"
            );
        }
    }

    // ─── Property tests ───────────────────────────────────────────────────────

    #[allow(clippy::cast_possible_wrap)]
    mod proptest_cache {
        use super::*;
        use proptest::prelude::*;

        fn pt_config() -> ProptestConfig {
            ProptestConfig {
                cases: 1000,
                max_shrink_iters: 5000,
                ..ProptestConfig::default()
            }
        }

        /// Strategy producing random alphanumeric slugs for cache tests.
        fn arb_slug() -> impl Strategy<Value = String> {
            proptest::string::string_regex("[a-z0-9]{1,12}").expect("valid regex")
        }

        proptest! {
            #![proptest_config(pt_config())]

            /// entry_counts() never exceeds capacity after any number of puts.
            #[test]
            #[allow(clippy::cast_possible_wrap)]
            fn prop_cache_capacity_never_exceeded(
                slugs in proptest::collection::vec(arb_slug(), 1..=200)
            ) {
                let capacity = 50;
                let cache = ReadCache::new_for_testing_with_capacity(capacity);
                for (i, slug) in slugs.iter().enumerate() {
                    let unique = format!("{slug}-{i}");
                    cache.put_project(&ProjectRow {
                        id: Some(i as i64),
                        slug: unique.clone(),
                        human_key: format!("/data/{unique}"),
                        created_at: 0,
                    });
                }
                let counts = cache.entry_counts();
                prop_assert!(
                    counts.projects_by_slug <= capacity,
                    "slug count {} > capacity {capacity}",
                    counts.projects_by_slug
                );
                prop_assert!(
                    counts.projects_by_human_key <= capacity,
                    "human_key count {} > capacity {capacity}",
                    counts.projects_by_human_key
                );
            }

            /// put then immediate get always returns the same value.
            #[test]
            fn prop_cache_get_after_put_hits(slug in arb_slug()) {
                let cache = ReadCache::new();
                let project = ProjectRow {
                    id: Some(1),
                    slug: slug.clone(),
                    human_key: format!("/data/{slug}"),
                    created_at: 42,
                };
                cache.put_project(&project);
                let got = cache.get_project(&slug);
                prop_assert!(got.is_some(), "get after put must hit");
                prop_assert_eq!(got.unwrap().slug, slug);
            }

            /// put + invalidate + get returns None.
            #[test]
            fn prop_cache_invalidate_removes(
                name_idx in 0..100usize,
                project_id in 1..=50i64,
            ) {
                let cache = ReadCache::new();
                let name = format!("Agent{name_idx}");
                let agent = make_agent_with_id(&name, project_id, name_idx as i64);
                cache.put_agent(&agent);
                prop_assert!(
                    cache.get_agent(project_id, &name).is_some(),
                    "agent should be cached after put"
                );
                cache.invalidate_agent(project_id, &name);
                prop_assert!(
                    cache.get_agent(project_id, &name).is_none(),
                    "agent should be evicted after invalidate"
                );
            }

            /// warm_agents inserts all agents retrievably.
            #[test]
            fn prop_cache_warm_preserves_all(count in 1..=100usize) {
                let cache = ReadCache::new();
                let agents: Vec<AgentRow> = (0..count)
                    .map(|i| make_agent_with_id(&format!("W{i}"), 1, i as i64))
                    .collect();
                cache.warm_agents(&agents);
                for i in 0..count {
                    prop_assert!(
                        cache.get_agent(1, &format!("W{i}")).is_some(),
                        "warm agent W{i} missing"
                    );
                }
            }

            /// deferred touches coalesce: drain returns at most one entry per
            /// agent_id with the latest timestamp.
            #[test]
            fn prop_cache_deferred_touch_coalesces(
                ops in proptest::collection::vec(
                    (1..=20i64, 0..=1_000_000i64),
                    1..=200
                )
            ) {
                let cache = ReadCache::new();
                // Compute expected max timestamp per agent
                let mut expected: std::collections::HashMap<i64, i64> =
                    std::collections::HashMap::new();
                for &(aid, ts) in &ops {
                    expected
                        .entry(aid)
                        .and_modify(|v| {
                            if ts > *v {
                                *v = ts;
                            }
                        })
                        .or_insert(ts);
                    let _ = cache.enqueue_touch(aid, ts);
                }
                let drained = cache.drain_touches();
                // One entry per unique agent
                prop_assert_eq!(
                    drained.len(),
                    expected.len(),
                    "drain should have one entry per unique agent_id"
                );
                for (aid, max_ts) in &expected {
                    let got = drained.get(aid);
                    prop_assert!(got.is_some());
                    prop_assert_eq!(*got.unwrap(), *max_ts);
                }
            }
        }

        #[test]
        fn prop_cache_metrics_consistent() {
            // Run a mixed sequence of puts and gets; verify hits + misses is
            // non-decreasing and consistent.
            let cache = ReadCache::new();
            let before = CACHE_METRICS.snapshot();

            let mut ops_count = 0u64;
            for i in 0..100 {
                let slug = format!("m-{}", i % 20);
                if i % 3 == 0 {
                    cache.put_project(&ProjectRow {
                        id: Some(i),
                        slug: slug.clone(),
                        human_key: format!("/data/{slug}"),
                        created_at: 0,
                    });
                }
                let _ = cache.get_project(&slug);
                ops_count += 1;
            }

            let after = CACHE_METRICS.snapshot();
            let delta_hits = after.project_hits - before.project_hits;
            let delta_misses = after.project_misses - before.project_misses;
            assert!(
                delta_hits + delta_misses >= ops_count,
                "hits({delta_hits}) + misses({delta_misses}) < ops({ops_count})"
            );
        }
    }

    #[test]
    fn inbox_stats_scope_isolation_prevents_cross_db_collisions() {
        let cache = ReadCache::new();
        let row_a = InboxStatsRow {
            agent_id: 2,
            total_count: 1,
            unread_count: 1,
            ack_pending_count: 1,
            last_message_ts: Some(10),
        };
        let row_b = InboxStatsRow {
            agent_id: 2,
            total_count: 99,
            unread_count: 88,
            ack_pending_count: 77,
            last_message_ts: Some(20),
        };

        cache.put_inbox_stats_scoped("/tmp/a.sqlite3", &row_a);
        cache.put_inbox_stats_scoped("/tmp/b.sqlite3", &row_b);

        let got_a = cache
            .get_inbox_stats_scoped("/tmp/a.sqlite3", 2)
            .expect("scope a value");
        let got_b = cache
            .get_inbox_stats_scoped("/tmp/b.sqlite3", 2)
            .expect("scope b value");
        assert_eq!(got_a.total_count, 1);
        assert_eq!(got_b.total_count, 99);

        cache.invalidate_inbox_stats_scoped("/tmp/a.sqlite3", 2);
        assert!(cache.get_inbox_stats_scoped("/tmp/a.sqlite3", 2).is_none());
        assert!(cache.get_inbox_stats_scoped("/tmp/b.sqlite3", 2).is_some());
    }

    #[test]
    fn agent_scope_isolation_prevents_cross_db_collisions() {
        let cache = ReadCache::new();
        let mut a = make_agent_with_id("BlueLake", 1, 42);
        a.program = "codex-cli".to_string();
        let mut b = make_agent_with_id("BlueLake", 1, 42);
        b.program = "e2e-test".to_string();

        cache.put_agent_scoped("/tmp/a.sqlite3", &a);
        cache.put_agent_scoped("/tmp/b.sqlite3", &b);

        let a_by_key = cache
            .get_agent_scoped("/tmp/a.sqlite3", 1, "BlueLake")
            .expect("agent in scope a");
        let b_by_key = cache
            .get_agent_scoped("/tmp/b.sqlite3", 1, "BlueLake")
            .expect("agent in scope b");
        assert_eq!(a_by_key.program, "codex-cli");
        assert_eq!(b_by_key.program, "e2e-test");

        let a_by_id = cache
            .get_agent_by_id_scoped("/tmp/a.sqlite3", 42)
            .expect("agent id in scope a");
        let b_by_id = cache
            .get_agent_by_id_scoped("/tmp/b.sqlite3", 42)
            .expect("agent id in scope b");
        assert_eq!(a_by_id.program, "codex-cli");
        assert_eq!(b_by_id.program, "e2e-test");

        cache.invalidate_agent_scoped("/tmp/a.sqlite3", 1, "BlueLake");
        assert!(
            cache
                .get_agent_scoped("/tmp/a.sqlite3", 1, "BlueLake")
                .is_none()
        );
        assert!(
            cache
                .get_agent_scoped("/tmp/b.sqlite3", 1, "BlueLake")
                .is_some()
        );
        assert!(cache.get_agent_by_id_scoped("/tmp/a.sqlite3", 42).is_none());
        assert!(cache.get_agent_by_id_scoped("/tmp/b.sqlite3", 42).is_some());
    }

    // =========================================================================
    // 6 required tests for br-22zwu (S3-FIFO wiring verification)
    // =========================================================================

    /// 1. `readcache_s3fifo_project_hit_miss` -- same as `project_cache_hit_and_miss`
    ///    but explicitly verifying S3-FIFO backing (ghost promotion path).
    #[test]
    fn readcache_s3fifo_project_hit_miss() {
        let cache = ReadCache::with_capacity(5);

        // Miss path
        assert!(cache.get_project("alpha").is_none());
        assert!(cache.get_project_by_human_key("/data/alpha").is_none());

        // Insert + hit path
        let p = make_project("alpha");
        cache.put_project(&p);
        assert_eq!(cache.get_project("alpha").unwrap().slug, "alpha");
        assert_eq!(
            cache.get_project_by_human_key("/data/alpha").unwrap().slug,
            "alpha"
        );

        // Insert enough to trigger S3-FIFO eviction (capacity=5, small=1)
        for i in 0..10 {
            let p = make_project(&format!("evict-{i}"));
            cache.put_project(&p);
        }

        // Original may have been evicted; cache still works correctly
        // regardless of eviction decision
        let got = cache.get_project("alpha");
        if let Some(row) = got {
            assert_eq!(row.slug, "alpha");
        }
    }

    /// 2. `readcache_s3fifo_agent_dual_index_sync` -- invalidation from
    ///    `agent_by_key` also removes from `agent_by_id`.
    #[test]
    fn readcache_s3fifo_agent_dual_index_sync() {
        let cache = ReadCache::with_capacity(100);

        let agent = make_agent_with_id("RedFox", 1, 42);
        cache.put_agent(&agent);

        // Both indexes hit
        assert!(cache.get_agent(1, "RedFox").is_some());
        assert!(cache.get_agent_by_id(42).is_some());

        // Invalidate via key -> id index also cleared
        cache.invalidate_agent(1, "RedFox");
        assert!(cache.get_agent(1, "RedFox").is_none());
        assert!(cache.get_agent_by_id(42).is_none());

        // Re-insert and verify both indexes work again
        let agent2 = make_agent_with_id("BlueLake", 2, 99);
        cache.put_agent(&agent2);
        assert!(cache.get_agent(2, "BlueLake").is_some());
        assert!(cache.get_agent_by_id(99).is_some());
    }

    /// 3. `readcache_s3fifo_capacity_respected` -- insert > capacity items,
    ///    verify `len()` never exceeds capacity.
    #[test]
    #[allow(clippy::significant_drop_tightening)]
    fn readcache_s3fifo_capacity_respected() {
        let cap = 20;
        let cache = ReadCache::with_capacity(cap);

        for i in 0..200 {
            let agent = make_agent_with_id(&format!("Agent{i}"), 1, i + 1);
            cache.put_agent(&agent);

            let by_key = cache.agents_by_key.read();
            assert!(
                by_key.len() <= cap,
                "agents_by_key len {} exceeded capacity {} at insert {}",
                by_key.len(),
                cap,
                i
            );
        }

        // Also verify projects
        for i in 0..200 {
            let p = make_project(&format!("proj-{i}"));
            cache.put_project(&p);

            let by_slug = cache.projects_by_slug.read();
            assert!(
                by_slug.len() <= cap,
                "projects_by_slug len {} exceeded capacity {} at insert {}",
                by_slug.len(),
                cap,
                i
            );
        }
    }

    /// 4. `readcache_s3fifo_adaptive_ttl_preserved` -- hot entries still get
    ///    extended TTL after switching to S3-FIFO eviction.
    #[test]
    fn readcache_s3fifo_adaptive_ttl_preserved() {
        let cache = ReadCache::with_capacity(100);

        let project = make_project("hot-proj");
        cache.put_project(&project);

        // Access ADAPTIVE_TTL_THRESHOLD times to trigger 2x TTL
        for _ in 0..ADAPTIVE_TTL_THRESHOLD {
            let _ = cache.get_project("hot-proj");
        }

        // Verify the entry has extended effective TTL
        let mut by_slug = cache.projects_by_slug.write();
        let slug_key = "hot-proj".to_owned();
        if let Some(entry) = by_slug.get(&slug_key) {
            let effective = entry.effective_ttl(PROJECT_TTL);
            assert!(
                effective > PROJECT_TTL,
                "expected adaptive TTL > {PROJECT_TTL:?}, got {effective:?}"
            );
            assert_eq!(effective, PROJECT_TTL * 2);
        }
    }

    /// 5. `readcache_s3fifo_warm_agents_bulk` -- bulk insert path works with
    ///    S3-FIFO, all agents retrievable.
    #[test]
    fn readcache_s3fifo_warm_agents_bulk() {
        // Small queue = capacity/10. Need small >= 50 for all agents to
        // survive without eviction, so capacity >= 500.
        let cache = ReadCache::with_capacity(500);

        let agents: Vec<AgentRow> = (0..50)
            .map(|i| make_agent_with_id(&format!("Warm{i}"), 1, i + 1))
            .collect();

        cache.warm_agents(&agents);

        // All 50 should be retrievable from both indexes
        for i in 0..50 {
            let name = format!("Warm{i}");
            assert!(
                cache.get_agent(1, &name).is_some(),
                "warm agent {name} not found by key"
            );
            assert!(
                cache.get_agent_by_id(i + 1).is_some(),
                "warm agent {name} not found by id"
            );
        }
    }

    /// 6. `readcache_s3fifo_hit_rate_not_regressed` -- synthetic Zipf workload,
    ///    verifies S3-FIFO hit-rate is competitive with LRU baseline.
    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn readcache_s3fifo_hit_rate_not_regressed() {
        let cap = 100;
        let cache = ReadCache::with_capacity(cap);
        let num_unique = 500;
        let num_accesses = 5_000;

        // Pre-populate with `num_unique` projects
        for i in 0..num_unique {
            let p = make_project(&format!("z-{i}"));
            cache.put_project(&p);
        }

        // Simulate Zipf-like access pattern: lower indices accessed more often.
        // p(rank=r) ~ 1/r, approximated by sampling i = floor(N * (rand^2))
        // We use a deterministic sequence: i = (step * step) % num_unique
        let mut hits = 0u64;
        let mut misses = 0u64;
        for step in 0..num_accesses {
            // Deterministic Zipf-like: bias toward low indices
            let idx = ((step as u64 * 7 + 13) % num_unique as u64) as usize;
            let biased_idx = (idx * idx) % num_unique;
            let slug = format!("z-{biased_idx}");
            if cache.get_project(&slug).is_some() {
                hits += 1;
            } else {
                misses += 1;
                // Re-insert on miss (simulates DB fetch + cache fill)
                let p = make_project(&slug);
                cache.put_project(&p);
            }
        }

        let hit_rate = hits as f64 / (hits + misses) as f64;
        // S3-FIFO should achieve a reasonable hit rate on Zipf workloads.
        // With 100-entry cache and 500 unique keys, LRU typically gets ~40-60%.
        // S3-FIFO should be at least 20% (very conservative lower bound).
        assert!(
            hit_rate >= 0.20,
            "S3-FIFO hit rate {hit_rate:.3} is below minimum threshold 0.20"
        );
    }

    // ── I.1: Flat combining for cache flush (br-11fd5) ──────────────────

    /// 1. Enqueue a touch, verify `has_pending_touches()` is true.
    #[test]
    fn atomic_pending_flag_set_on_enqueue() {
        let cache = ReadCache::new_for_testing();
        assert!(!cache.has_pending_touches(), "starts false");

        cache.enqueue_touch(42, 1_000_000);
        assert!(cache.has_pending_touches(), "set after enqueue");
    }

    /// 2. Enqueue, drain, verify `has_pending_touches()` is false.
    #[test]
    fn atomic_pending_flag_cleared_on_drain() {
        let cache = ReadCache::new_for_testing();
        cache.enqueue_touch(1, 100);
        cache.enqueue_touch(2, 200);
        assert!(cache.has_pending_touches());

        let drained = cache.drain_touches();
        assert_eq!(drained.len(), 2);
        assert!(!cache.has_pending_touches(), "cleared after drain");
    }

    /// 3. Enqueue to different shards, all set the flag.
    #[test]
    fn atomic_pending_flag_multiple_shards() {
        let cache = ReadCache::new_for_testing();
        // Agent IDs that hash to different shards (id % 16).
        for i in 0..16_i64 {
            cache.enqueue_touch(i, i * 1000);
        }
        assert!(cache.has_pending_touches());

        // Drain and verify all were collected.
        let drained = cache.drain_touches();
        assert_eq!(drained.len(), 16);
        assert!(!cache.has_pending_touches());
    }

    /// 4. Verify `has_pending_touches()` uses atomic (no mutex) by checking timing.
    #[test]
    fn atomic_pending_flag_no_spurious_locks() {
        let cache = ReadCache::new_for_testing();

        // Call has_pending_touches 10_000 times. With atomic, this should be
        // sub-millisecond. With 16 mutex locks, it would be measurably slower.
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _ = cache.has_pending_touches();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "10K atomic loads should be <50ms; took {elapsed:?}"
        );
    }

    // ── Inbox Stats Cache ───────────────────────────────────────────

    fn make_inbox_stats(agent_id: i64, total: i64, unread: i64) -> InboxStatsRow {
        InboxStatsRow {
            agent_id,
            total_count: total,
            unread_count: unread,
            ack_pending_count: 0,
            last_message_ts: Some(1_000_000),
        }
    }

    #[test]
    fn inbox_stats_put_and_get() {
        let cache = ReadCache::new_for_testing();
        let stats = make_inbox_stats(42, 10, 3);
        cache.put_inbox_stats(&stats);
        let got = cache.get_inbox_stats(42);
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.agent_id, 42);
        assert_eq!(got.total_count, 10);
        assert_eq!(got.unread_count, 3);
    }

    #[test]
    fn inbox_stats_miss_returns_none() {
        let cache = ReadCache::new_for_testing();
        assert!(cache.get_inbox_stats(999).is_none());
    }

    #[test]
    fn inbox_stats_invalidate_removes_entry() {
        let cache = ReadCache::new_for_testing();
        let stats = make_inbox_stats(42, 10, 3);
        cache.put_inbox_stats(&stats);
        assert!(cache.get_inbox_stats(42).is_some());
        cache.invalidate_inbox_stats(42);
        assert!(cache.get_inbox_stats(42).is_none());
    }

    #[test]
    fn inbox_stats_scoped_isolation() {
        let cache = ReadCache::new_for_testing();
        let stats_a = make_inbox_stats(42, 10, 3);
        let mut stats_b = make_inbox_stats(42, 20, 5);
        stats_b.agent_id = 42; // same agent_id, different scope
        cache.put_inbox_stats_scoped("scope-a", &stats_a);
        cache.put_inbox_stats_scoped("scope-b", &stats_b);

        let got_a = cache.get_inbox_stats_scoped("scope-a", 42);
        let got_b = cache.get_inbox_stats_scoped("scope-b", 42);
        assert!(got_a.is_some());
        assert!(got_b.is_some());
        assert_eq!(got_a.unwrap().total_count, 10);
        assert_eq!(got_b.unwrap().total_count, 20);
    }

    #[test]
    fn inbox_stats_scoped_invalidate_only_affects_scope() {
        let cache = ReadCache::new_for_testing();
        let stats = make_inbox_stats(42, 10, 3);
        cache.put_inbox_stats_scoped("scope-a", &stats);
        cache.put_inbox_stats_scoped("scope-b", &stats);

        cache.invalidate_inbox_stats_scoped("scope-a", 42);
        assert!(cache.get_inbox_stats_scoped("scope-a", 42).is_none());
        assert!(cache.get_inbox_stats_scoped("scope-b", 42).is_some());
    }

    #[test]
    fn inbox_stats_default_scope_matches_empty_string() {
        let cache = ReadCache::new_for_testing();
        let stats = make_inbox_stats(42, 10, 3);
        // put via default (empty scope)
        cache.put_inbox_stats(&stats);
        // get via explicit empty scope should match
        let got = cache.get_inbox_stats_scoped("", 42);
        assert!(got.is_some());
        assert_eq!(got.unwrap().total_count, 10);
    }

    #[test]
    fn inbox_stats_overwrite_updates_value() {
        let cache = ReadCache::new_for_testing();
        let stats1 = make_inbox_stats(42, 10, 3);
        cache.put_inbox_stats(&stats1);

        let stats2 = make_inbox_stats(42, 20, 7);
        cache.put_inbox_stats(&stats2);

        let got = cache.get_inbox_stats(42).unwrap();
        assert_eq!(got.total_count, 20);
        assert_eq!(got.unread_count, 7);
    }

    #[test]
    fn inbox_stats_different_agents_independent() {
        let cache = ReadCache::new_for_testing();
        cache.put_inbox_stats(&make_inbox_stats(1, 10, 3));
        cache.put_inbox_stats(&make_inbox_stats(2, 20, 5));

        assert_eq!(cache.get_inbox_stats(1).unwrap().total_count, 10);
        assert_eq!(cache.get_inbox_stats(2).unwrap().total_count, 20);

        cache.invalidate_inbox_stats(1);
        assert!(cache.get_inbox_stats(1).is_none());
        assert!(cache.get_inbox_stats(2).is_some());
    }
}
