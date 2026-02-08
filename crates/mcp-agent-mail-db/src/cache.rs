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
//! ## LRU Eviction
//!
//! Uses `IndexMap` for O(1) LRU eviction: entries are ordered by insertion/access
//! time, and on capacity overflow the oldest (front) entries are evicted.
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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use crate::models::{AgentRow, InboxStatsRow, ProjectRow};
use mcp_agent_mail_core::{InternedStr, LockLevel, OrderedMutex, OrderedRwLock};

const PROJECT_TTL: Duration = Duration::from_secs(300); // 5 min
const AGENT_TTL: Duration = Duration::from_secs(300); // 5 min
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

/// In-memory read cache for projects and agents.
pub struct ReadCache {
    projects_by_slug: OrderedRwLock<IndexMap<String, CacheEntry<ProjectRow>>>,
    projects_by_human_key: OrderedRwLock<IndexMap<String, CacheEntry<ProjectRow>>>,
    agents_by_key: OrderedRwLock<IndexMap<(i64, InternedStr), CacheEntry<AgentRow>>>,
    agents_by_id: OrderedRwLock<IndexMap<i64, CacheEntry<AgentRow>>>,
    /// Sharded deferred touch queue (16 shards, keyed by `agent_id % 16`).
    /// Each shard maps `agent_id` → latest requested timestamp (micros).
    deferred_touch_shards: [OrderedMutex<HashMap<i64, i64>>; NUM_TOUCH_SHARDS],
    /// Last time we flushed the deferred touches.
    last_touch_flush: OrderedMutex<Instant>,
}

impl ReadCache {
    fn new() -> Self {
        Self {
            projects_by_slug: OrderedRwLock::new(
                LockLevel::DbReadCacheProjectsBySlug,
                IndexMap::new(),
            ),
            projects_by_human_key: OrderedRwLock::new(
                LockLevel::DbReadCacheProjectsByHumanKey,
                IndexMap::new(),
            ),
            agents_by_key: OrderedRwLock::new(LockLevel::DbReadCacheAgentsByKey, IndexMap::new()),
            agents_by_id: OrderedRwLock::new(LockLevel::DbReadCacheAgentsById, IndexMap::new()),
            deferred_touch_shards: std::array::from_fn(|_| {
                OrderedMutex::new(LockLevel::DbReadCacheDeferredTouches, HashMap::new())
            }),
            last_touch_flush: OrderedMutex::new(
                LockLevel::DbReadCacheLastTouchFlush,
                Instant::now(),
            ),
        }
    }

    // -------------------------------------------------------------------------
    // Project cache
    // -------------------------------------------------------------------------

    /// Look up a project by slug. Returns `None` if not cached or expired.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_project(&self, slug: &str) -> Option<ProjectRow> {
        let mut map = self.projects_by_slug.write();
        let Some(idx) = map.get_index_of(slug) else {
            CACHE_METRICS.record_project_miss();
            return None;
        };
        // Check expiry and clone value in a scoped borrow
        let (expired, value) = {
            let (_, entry) = map.get_index(idx).unwrap();
            (entry.is_expired(PROJECT_TTL), entry.value.clone())
        };
        if expired {
            map.shift_remove_index(idx);
            CACHE_METRICS.record_project_miss();
            return None;
        }
        // Touch and move to back for LRU ordering
        map.get_index_mut(idx).unwrap().1.touch();
        let last = map.len() - 1;
        map.move_index(idx, last);
        CACHE_METRICS.record_project_hit();
        Some(value)
    }

    /// Look up a project by `human_key`.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_project_by_human_key(&self, human_key: &str) -> Option<ProjectRow> {
        let mut map = self.projects_by_human_key.write();
        let Some(idx) = map.get_index_of(human_key) else {
            CACHE_METRICS.record_project_miss();
            return None;
        };
        let (expired, value) = {
            let (_, entry) = map.get_index(idx).unwrap();
            (entry.is_expired(PROJECT_TTL), entry.value.clone())
        };
        if expired {
            map.shift_remove_index(idx);
            CACHE_METRICS.record_project_miss();
            return None;
        }
        map.get_index_mut(idx).unwrap().1.touch();
        let last = map.len() - 1;
        map.move_index(idx, last);
        CACHE_METRICS.record_project_hit();
        Some(value)
    }

    /// Cache a project (write-through after DB mutation).
    /// Indexes by both `slug` and `human_key`.
    pub fn put_project(&self, project: &ProjectRow) {
        // Index by slug
        {
            let mut map = self.projects_by_slug.write();
            lru_evict_if_full(&mut map, PROJECT_TTL);
            map.insert(project.slug.clone(), CacheEntry::new(project.clone()));
        }
        // Index by human_key
        {
            let mut map = self.projects_by_human_key.write();
            lru_evict_if_full(&mut map, PROJECT_TTL);
            map.insert(project.human_key.clone(), CacheEntry::new(project.clone()));
        }
    }

    // -------------------------------------------------------------------------
    // Agent cache
    // -------------------------------------------------------------------------

    /// Look up an agent by (`project_id`, name). Returns `None` if not cached or expired.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent(&self, project_id: i64, name: &str) -> Option<AgentRow> {
        let key = (project_id, InternedStr::new(name));
        let mut map = self.agents_by_key.write();
        let Some(idx) = map.get_index_of(&key) else {
            CACHE_METRICS.record_agent_miss();
            return None;
        };
        let (expired, value) = {
            let (_, entry) = map.get_index(idx).unwrap();
            (entry.is_expired(AGENT_TTL), entry.value.clone())
        };
        if expired {
            map.shift_remove_index(idx);
            CACHE_METRICS.record_agent_miss();
            return None;
        }
        map.get_index_mut(idx).unwrap().1.touch();
        let last = map.len() - 1;
        map.move_index(idx, last);
        CACHE_METRICS.record_agent_hit();
        Some(value)
    }

    /// Look up an agent by id.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_agent_by_id(&self, agent_id: i64) -> Option<AgentRow> {
        let mut map = self.agents_by_id.write();
        let Some(idx) = map.get_index_of(&agent_id) else {
            CACHE_METRICS.record_agent_miss();
            return None;
        };
        let (expired, value) = {
            let (_, entry) = map.get_index(idx).unwrap();
            (entry.is_expired(AGENT_TTL), entry.value.clone())
        };
        if expired {
            map.shift_remove_index(idx);
            CACHE_METRICS.record_agent_miss();
            return None;
        }
        map.get_index_mut(idx).unwrap().1.touch();
        let last = map.len() - 1;
        map.move_index(idx, last);
        CACHE_METRICS.record_agent_hit();
        Some(value)
    }

    /// Cache an agent (write-through after DB mutation).
    /// Indexes by both (`project_id`, `name`) and `id`.
    pub fn put_agent(&self, agent: &AgentRow) {
        // Index by (project_id, name)
        {
            let mut map = self.agents_by_key.write();
            lru_evict_if_full_tuple(&mut map, AGENT_TTL);
            map.insert(
                (agent.project_id, InternedStr::new(&agent.name)),
                CacheEntry::new(agent.clone()),
            );
        }
        // Index by id (if present)
        if let Some(id) = agent.id {
            let mut map = self.agents_by_id.write();
            lru_evict_if_full_i64(&mut map, AGENT_TTL);
            map.insert(id, CacheEntry::new(agent.clone()));
        }
    }

    /// Bulk-insert agents into the cache (cache warming on startup).
    /// Useful for pre-loading all agents for active projects to avoid cold-start
    /// DB round-trips.
    pub fn warm_agents(&self, agents: &[AgentRow]) {
        {
            let mut by_key = self.agents_by_key.write();
            for agent in agents {
                by_key.insert(
                    (agent.project_id, InternedStr::new(&agent.name)),
                    CacheEntry::new(agent.clone()),
                );
            }
        }
        {
            let mut by_id = self.agents_by_id.write();
            for agent in agents {
                if let Some(id) = agent.id {
                    by_id.insert(id, CacheEntry::new(agent.clone()));
                }
            }
        }
    }

    /// Bulk-insert projects into the cache (cache warming on startup).
    pub fn warm_projects(&self, projects: &[ProjectRow]) {
        {
            let mut by_slug = self.projects_by_slug.write();
            for project in projects {
                by_slug.insert(project.slug.clone(), CacheEntry::new(project.clone()));
            }
        }
        {
            let mut by_key = self.projects_by_human_key.write();
            for project in projects {
                by_key.insert(project.human_key.clone(), CacheEntry::new(project.clone()));
            }
        }
    }

    /// Invalidate a specific agent entry (call after `register_agent` update).
    pub fn invalidate_agent(&self, project_id: i64, name: &str) {
        let mut map = self.agents_by_key.write();
        if let Some(entry) = map.shift_remove(&(project_id, InternedStr::new(name))) {
            // Also remove from id index
            if let Some(id) = entry.value.id {
                drop(map); // release key map lock first
                let mut id_map = self.agents_by_id.write();
                id_map.shift_remove(&id);
            }
        }
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

        let last = self.last_touch_flush.lock();
        last.elapsed() >= TOUCH_FLUSH_INTERVAL
    }

    /// Drain all pending touch entries from all shards and reset the flush clock.
    /// Returns the merged map of `agent_id` → latest timestamp.
    pub fn drain_touches(&self) -> HashMap<i64, i64> {
        let mut merged = HashMap::new();
        for shard in &self.deferred_touch_shards {
            let mut s = shard.lock();
            merged.extend(s.drain());
        }
        let mut last = self.last_touch_flush.lock();
        *last = Instant::now();
        merged
    }

    /// Check if there are pending touches in any shard.
    pub fn has_pending_touches(&self) -> bool {
        self.deferred_touch_shards
            .iter()
            .any(|shard| !shard.lock().is_empty())
    }

    /// Return current entry counts per cache category.
    pub fn entry_counts(&self) -> CacheEntryCounts {
        CacheEntryCounts {
            projects_by_slug: self.projects_by_slug.read().len(),
            projects_by_human_key: self.projects_by_human_key.read().len(),
            agents_by_key: self.agents_by_key.read().len(),
            agents_by_id: self.agents_by_id.read().len(),
        }
    }

    /// Create a new standalone cache instance (for testing).
    #[must_use]
    pub fn new_for_testing() -> Self {
        Self::new()
    }

    /// Clear all cache entries (for testing).
    #[cfg(test)]
    pub fn clear(&self) {
        self.projects_by_slug.write().clear();
        self.projects_by_human_key.write().clear();
        self.agents_by_key.write().clear();
        self.agents_by_id.write().clear();
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
}

/// LRU eviction for `IndexMap<String, CacheEntry<T>>`:
/// 1. First remove expired entries.
/// 2. If still at capacity, evict the oldest (front) entries until below capacity.
fn lru_evict_if_full<T>(map: &mut IndexMap<String, CacheEntry<T>>, ttl: Duration) {
    if map.len() < MAX_ENTRIES_PER_CATEGORY {
        return;
    }
    // Phase 1: evict expired
    map.retain(|_, entry| !entry.is_expired(ttl));
    // Phase 2: LRU eviction from the front if still at capacity
    while map.len() >= MAX_ENTRIES_PER_CATEGORY {
        map.shift_remove_index(0);
    }
}

/// LRU eviction for `IndexMap<(i64, InternedStr), CacheEntry<T>>`.
fn lru_evict_if_full_tuple<T>(
    map: &mut IndexMap<(i64, InternedStr), CacheEntry<T>>,
    ttl: Duration,
) {
    if map.len() < MAX_ENTRIES_PER_CATEGORY {
        return;
    }
    map.retain(|_, entry| !entry.is_expired(ttl));
    while map.len() >= MAX_ENTRIES_PER_CATEGORY {
        map.shift_remove_index(0);
    }
}

/// LRU eviction for `IndexMap<i64, CacheEntry<T>>`.
fn lru_evict_if_full_i64<T>(map: &mut IndexMap<i64, CacheEntry<T>>, ttl: Duration) {
    if map.len() < MAX_ENTRIES_PER_CATEGORY {
        return;
    }
    map.retain(|_, entry| !entry.is_expired(ttl));
    while map.len() >= MAX_ENTRIES_PER_CATEGORY {
        map.shift_remove_index(0);
    }
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
    fn lru_eviction_order() {
        // Verify that LRU eviction removes the oldest (front) entries first.
        let cache = ReadCache::new();

        // Fill to capacity with agents
        for i in 0..MAX_ENTRIES_PER_CATEGORY {
            let name = format!("Agent{i}");
            let agent_id = i64::try_from(i).unwrap_or(i64::MAX);
            cache.put_agent(&make_agent_with_id(&name, 1, agent_id));
        }

        assert_eq!(cache.agents_by_key.read().len(), MAX_ENTRIES_PER_CATEGORY);

        // Access Agent0 so it moves to back (recently used)
        let _ = cache.get_agent(1, "Agent0");

        // Insert one more to trigger eviction
        cache.put_agent(&make_agent_with_id("NewAgent", 1, 99999));

        let (has_agent0, has_agent1, has_new_agent) = {
            let map = cache.agents_by_key.read();
            (
                map.contains_key(&(1_i64, InternedStr::new("Agent0"))),
                map.contains_key(&(1_i64, InternedStr::new("Agent1"))),
                map.contains_key(&(1_i64, InternedStr::new("NewAgent"))),
            )
        };
        // Agent0 should still be present (was recently accessed, moved to back)
        assert!(has_agent0);
        // Agent1 should be evicted (was at the front after Agent0 moved to back)
        assert!(!has_agent1);
        // NewAgent should be present
        assert!(has_new_agent);
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

        let base = Duration::from_secs(60);
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
        let cache = ReadCache::new();
        for i in 0..2000 {
            let name = format!("Agent{i}");
            cache.put_agent(&make_agent_with_id(&name, 1, i));
        }
        let counts = cache.entry_counts();
        assert!(counts.agents_by_key <= MAX_ENTRIES_PER_CATEGORY);
        assert!(counts.agents_by_id <= MAX_ENTRIES_PER_CATEGORY);
        // All 2000 should fit since MAX is 16,384
        assert_eq!(counts.agents_by_key, 2000);
    }

    #[test]
    fn access_bumps_count() {
        let cache = ReadCache::new();
        cache.put_agent(&make_agent("HotAgent", 1));

        // Access 10 times
        for _ in 0..10 {
            let _ = cache.get_agent(1, "HotAgent");
        }

        let access_count = {
            let map = cache.agents_by_key.read();
            map.get(&(1_i64, InternedStr::new("HotAgent")))
                .map(|entry| entry.access_count)
                .unwrap_or_default()
        };
        assert_eq!(access_count, 10);
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
}
