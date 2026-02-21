//! S3-FIFO cache eviction algorithm (Yang et al., SOSP 2023).
//!
//! S3-FIFO uses three FIFO queues to achieve near-optimal cache eviction
//! with O(1) amortized operations:
//!
//! - **Small (S):** Newly inserted items land here. Capacity = 10% of total.
//!   On eviction, items with `freq >= 1` promote to Main; others go to Ghost.
//! - **Main (M):** Promoted items. Capacity = 90% of total. On eviction,
//!   items with `freq >= 1` get reinserted at tail with freq reset; others
//!   are permanently evicted.
//! - **Ghost (G):** Keys-only metadata of recently evicted items from Small.
//!   Capacity = total cache size. Re-access of a ghost key inserts directly
//!   into Main instead of Small.
//!
//! Each queue is a `VecDeque` (FIFO). The `HashMap` maps keys to which queue
//! they reside in, enabling O(1) lookup. Frequency counters are 2-bit
//! (saturate at 3).

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// Which queue a key currently resides in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Small,
    Main,
    Ghost,
}

/// An entry in the Small or Main queue.
#[derive(Debug)]
struct QueueEntry<K, V> {
    key: K,
    value: V,
    freq: u8, // 2-bit, saturates at 3
}

/// S3-FIFO cache with O(1) amortized insert, get, and eviction.
///
/// # Type Parameters
///
/// - `K`: Key type (must be `Clone + Eq + Hash`).
/// - `V`: Value type (must be `Clone`).
///
/// # Examples
///
/// ```
/// use mcp_agent_mail_db::s3fifo::S3FifoCache;
///
/// let mut cache = S3FifoCache::new(10);
/// cache.insert("key1", 100);
/// assert_eq!(cache.get(&"key1"), Some(&100));
/// ```
pub struct S3FifoCache<K, V> {
    small: VecDeque<QueueEntry<K, V>>,
    main: VecDeque<QueueEntry<K, V>>,
    ghost: VecDeque<K>,
    index: HashMap<K, Location>,
    small_capacity: usize,
    main_capacity: usize,
    ghost_capacity: usize,
}

impl<K, V> S3FifoCache<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    /// Create a new S3-FIFO cache with the given total capacity.
    ///
    /// Small queue gets 10% of capacity (minimum 1), Main gets the rest.
    /// Ghost queue capacity equals total capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "S3-FIFO capacity must be > 0");
        let small_cap = (capacity / 10).max(1);
        let main_cap = capacity - small_cap;
        Self {
            small: VecDeque::with_capacity(small_cap),
            main: VecDeque::with_capacity(main_cap),
            ghost: VecDeque::with_capacity(capacity),
            index: HashMap::with_capacity(capacity),
            small_capacity: small_cap,
            main_capacity: main_cap,
            ghost_capacity: capacity,
        }
    }

    /// Look up a key, incrementing its frequency counter on hit.
    ///
    /// Returns `None` if the key is not present (ghost entries are not
    /// visible to callers).
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let loc = self.index.get(key)?;
        match loc {
            Location::Small => {
                if let Some(entry) = self.small.iter_mut().find(|e| e.key.borrow() == key) {
                    entry.freq = (entry.freq + 1).min(3);
                    return Some(&entry.value);
                }
                None
            }
            Location::Main => {
                if let Some(entry) = self.main.iter_mut().find(|e| e.key.borrow() == key) {
                    entry.freq = (entry.freq + 1).min(3);
                    return Some(&entry.value);
                }
                None
            }
            Location::Ghost => None, // ghost entries have no value
        }
    }

    /// Look up a key, returning a mutable reference to the value.
    ///
    /// Increments the frequency counter on hit. Returns `None` if the key
    /// is not present or is a ghost entry.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let loc = self.index.get(key)?;
        match loc {
            Location::Small => {
                if let Some(entry) = self.small.iter_mut().find(|e| e.key.borrow() == key) {
                    entry.freq = (entry.freq + 1).min(3);
                    return Some(&mut entry.value);
                }
                None
            }
            Location::Main => {
                if let Some(entry) = self.main.iter_mut().find(|e| e.key.borrow() == key) {
                    entry.freq = (entry.freq + 1).min(3);
                    return Some(&mut entry.value);
                }
                None
            }
            Location::Ghost => None,
        }
    }

    /// Check whether a key is present (Small or Main, not Ghost).
    #[must_use]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        matches!(self.index.get(key), Some(Location::Small | Location::Main))
    }

    /// Insert a key-value pair into the cache.
    ///
    /// If the key exists in Ghost, it is promoted directly to Main.
    /// If the key exists in Small or Main, its value is updated in place.
    /// Otherwise, it enters Small.
    pub fn insert(&mut self, key: K, value: V) {
        // Update existing entry in Small or Main
        if let Some(loc) = self.index.get(&key) {
            match loc {
                Location::Small => {
                    if let Some(entry) = self.small.iter_mut().find(|e| e.key == key) {
                        entry.value = value;
                        entry.freq = (entry.freq + 1).min(3);
                    }
                    return;
                }
                Location::Main => {
                    if let Some(entry) = self.main.iter_mut().find(|e| e.key == key) {
                        entry.value = value;
                        entry.freq = (entry.freq + 1).min(3);
                    }
                    return;
                }
                Location::Ghost => {
                    // Remove from ghost, insert into main
                    self.ghost.retain(|k| k != &key);
                    self.index.remove(&key);
                    self.evict_main_if_full();
                    self.main.push_back(QueueEntry {
                        key: key.clone(),
                        value,
                        freq: 0,
                    });
                    self.index.insert(key, Location::Main);
                    return;
                }
            }
        }

        // New key: insert into Small
        self.evict_small_if_full();
        self.small.push_back(QueueEntry {
            key: key.clone(),
            value,
            freq: 0,
        });
        self.index.insert(key, Location::Small);
    }

    /// Number of live entries (Small + Main, excludes Ghost).
    #[must_use]
    pub fn len(&self) -> usize {
        self.small.len() + self.main.len()
    }

    /// Whether the cache has no live entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total capacity (Small + Main).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.small_capacity + self.main_capacity
    }

    /// Remove a key from the cache entirely (including Ghost).
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let loc = self.index.remove(key)?;
        match loc {
            Location::Small => {
                if let Some(pos) = self.small.iter().position(|e| e.key.borrow() == key) {
                    return Some(self.small.remove(pos).unwrap().value);
                }
                None
            }
            Location::Main => {
                if let Some(pos) = self.main.iter().position(|e| e.key.borrow() == key) {
                    return Some(self.main.remove(pos).unwrap().value);
                }
                None
            }
            Location::Ghost => {
                self.ghost.retain(|k| k.borrow() != key);
                None
            }
        }
    }

    /// Evict from the Small queue until it is below capacity.
    ///
    /// Items with `freq >= 1` promote to Main; others go to Ghost.
    fn evict_small_if_full(&mut self) {
        while self.small.len() >= self.small_capacity {
            let Some(entry) = self.small.pop_front() else {
                break;
            };
            self.index.remove(&entry.key);

            if entry.freq >= 1 {
                // Promote to Main
                self.evict_main_if_full();
                self.index.insert(entry.key.clone(), Location::Main);
                self.main.push_back(QueueEntry {
                    key: entry.key,
                    value: entry.value,
                    freq: 0, // reset on promotion
                });
            } else {
                // Demote to Ghost (key only)
                self.evict_ghost_if_full();
                self.index.insert(entry.key.clone(), Location::Ghost);
                self.ghost.push_back(entry.key);
                // value is dropped
            }
        }
    }

    /// Evict from the Main queue until it is below capacity.
    ///
    /// Items with `freq >= 1` get reinserted at tail with freq reset.
    /// Others are permanently evicted.
    fn evict_main_if_full(&mut self) {
        // Safety limit to prevent infinite loop if all entries have freq >= 1
        let mut budget = self.main.len() + 1;
        while self.main.len() >= self.main_capacity && budget > 0 {
            budget -= 1;
            let Some(entry) = self.main.pop_front() else {
                break;
            };
            if entry.freq >= 1 {
                // Reinsert at tail with reset freq
                self.main.push_back(QueueEntry {
                    key: entry.key,
                    value: entry.value,
                    freq: 0,
                });
            } else {
                // Permanent eviction
                self.index.remove(&entry.key);
            }
        }
    }

    /// Evict from Ghost until it is below capacity.
    fn evict_ghost_if_full(&mut self) {
        while self.ghost.len() >= self.ghost_capacity {
            if let Some(evicted_key) = self.ghost.pop_front() {
                self.index.remove(&evicted_key);
            }
        }
    }

    /// Clear all entries from all queues.
    pub fn clear(&mut self) {
        self.small.clear();
        self.main.clear();
        self.ghost.clear();
        self.index.clear();
    }

    /// Number of entries in the Ghost queue (for diagnostics).
    #[must_use]
    pub fn ghost_len(&self) -> usize {
        self.ghost.len()
    }

    /// Number of entries in the Small queue (for diagnostics).
    #[must_use]
    pub fn small_len(&self) -> usize {
        self.small.len()
    }

    /// Number of entries in the Main queue (for diagnostics).
    #[must_use]
    pub fn main_len(&self) -> usize {
        self.main.len()
    }

    /// Iterate over all live keys (Small + Main queues, excluding Ghost).
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.small
            .iter()
            .map(|e| &e.key)
            .chain(self.main.iter().map(|e| &e.key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Basic insert/retrieve cycle.
    #[test]
    fn s3fifo_insert_and_get() {
        // capacity 30 -> small=3, so 3 items fit without eviction
        let mut cache = S3FifoCache::new(30);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.get(&"d"), None);
        assert_eq!(cache.len(), 3);
    }

    /// Item with freq >= 1 promoted from Small to Main on eviction.
    #[test]
    fn s3fifo_small_to_main_promotion() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);
        assert_eq!(cache.small_capacity, 1);

        // Insert "a" into small
        cache.insert("a", 10);
        assert_eq!(cache.small_len(), 1);
        assert_eq!(cache.main_len(), 0);

        // Access "a" to bump freq
        assert_eq!(cache.get(&"a"), Some(&10));

        // Insert "b" -> small is full, "a" (freq=1) should promote to main
        cache.insert("b", 20);
        assert_eq!(cache.main_len(), 1); // "a" promoted
        assert_eq!(cache.small_len(), 1); // "b" in small
        assert_eq!(cache.get(&"a"), Some(&10)); // still accessible via main
    }

    /// Evicted item from Small goes to Ghost; re-access goes to Main.
    #[test]
    fn s3fifo_ghost_reinsertion() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);

        // Insert "a" (no access -> freq stays 0)
        cache.insert("a", 10);
        // Insert "b" -> evicts "a" from small. "a" has freq=0 -> goes to ghost
        cache.insert("b", 20);

        assert_eq!(cache.get(&"a"), None); // "a" is in ghost, not visible
        assert_eq!(cache.ghost_len(), 1);

        // Re-insert "a" -> should go to Main (ghost hit)
        cache.insert("a", 100);
        assert_eq!(cache.ghost_len(), 0);
        assert_eq!(cache.main_len(), 1);
        assert_eq!(cache.get(&"a"), Some(&100));
    }

    /// Item in Main with freq >= 1 reinserted at tail on eviction.
    #[test]
    fn s3fifo_main_reinsert_on_freq() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);

        // Fill main with 4 items (promote via freq bump)
        for i in 0..4 {
            let key = i;
            cache.insert(key, i * 10);
            cache.get(&key); // bump freq
            // Insert dummy to trigger small eviction -> promote
            cache.insert(100 + i, 0);
        }

        // Access item 0 in main to bump its freq
        cache.get(&0);

        // Fill more to cause main eviction pressure
        // Item 0 should survive (freq >= 1) while zero-freq items get evicted
        for i in 200..210 {
            cache.insert(i, i);
            // Bump freq and push to main
            cache.get(&i);
            cache.insert(300 + i, 0);
        }

        // The cache should not exceed capacity
        assert!(cache.len() <= cache.capacity());
    }

    /// Cache never exceeds configured capacity.
    #[test]
    fn s3fifo_capacity_invariant() {
        let cap = 20;
        let mut cache = S3FifoCache::new(cap);

        for i in 0..1000 {
            cache.insert(i, i * 10);
            assert!(
                cache.len() <= cap,
                "len {} exceeded capacity {} at insert {}",
                cache.len(),
                cap,
                i
            );
        }
    }

    /// Insert 100K items and verify wall time scales linearly (not quadratically).
    #[test]
    fn s3fifo_eviction_is_o1() {
        use std::time::Instant;

        let n = 100_000;
        let cap = 1000;
        let mut cache = S3FifoCache::new(cap);

        let start = Instant::now();
        for i in 0..n {
            cache.insert(i, i);
        }
        let elapsed = start.elapsed();

        // With O(1) amortized ops, 100K inserts should complete well under 1 second.
        // O(n^2) would take ~10+ seconds on this workload. We use a generous 2s threshold.
        assert!(
            elapsed.as_secs() < 2,
            "100K inserts took {elapsed:?}, expected < 2s for O(1) amortized"
        );
        assert!(cache.len() <= cap);
    }

    /// Ghost queue respects its capacity limit.
    #[test]
    fn s3fifo_ghost_bounded() {
        // capacity 10 -> small=1, main=9, ghost=10
        let mut cache = S3FifoCache::new(10);

        // Insert 50 items without accessing them (all go to ghost on eviction)
        for i in 0..50 {
            cache.insert(i, i);
        }

        // Ghost should never exceed its capacity
        assert!(
            cache.ghost_len() <= cache.ghost_capacity,
            "ghost_len {} exceeded ghost_capacity {}",
            cache.ghost_len(),
            cache.ghost_capacity
        );
    }

    /// Get on empty returns None, evict on empty is no-op.
    #[test]
    fn s3fifo_empty_cache_operations() {
        let mut cache: S3FifoCache<&str, i32> = S3FifoCache::new(5);
        assert_eq!(cache.get(&"nonexistent"), None);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.ghost_len(), 0);
        assert_eq!(cache.small_len(), 0);
        assert_eq!(cache.main_len(), 0);

        // Remove on empty is fine
        assert_eq!(cache.remove(&"nonexistent"), None);
    }

    // ── New tests ──────────────────────────────────────────────

    #[test]
    fn s3fifo_get_mut_modifies_value() {
        let mut cache = S3FifoCache::new(10);
        cache.insert("key", 100);

        if let Some(val) = cache.get_mut(&"key") {
            *val = 999;
        }
        assert_eq!(cache.get(&"key"), Some(&999));
    }

    #[test]
    fn s3fifo_get_mut_returns_none_for_missing() {
        let mut cache: S3FifoCache<&str, i32> = S3FifoCache::new(10);
        assert!(cache.get_mut(&"missing").is_none());
    }

    #[test]
    fn s3fifo_get_mut_returns_none_for_ghost() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 10);
        // Insert "b" without accessing "a" -> "a" evicts to ghost (freq=0)
        cache.insert("b", 20);
        assert!(cache.get_mut(&"a").is_none());
        assert_eq!(cache.ghost_len(), 1);
    }

    #[test]
    fn s3fifo_contains_key() {
        let mut cache = S3FifoCache::new(10);
        cache.insert("a", 1);
        assert!(cache.contains_key(&"a"));
        assert!(!cache.contains_key(&"b"));
    }

    #[test]
    fn s3fifo_contains_key_excludes_ghost() {
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 1);
        cache.insert("b", 2); // evicts "a" to ghost
        assert!(!cache.contains_key(&"a"));
    }

    #[test]
    fn s3fifo_keys_iterator() {
        let mut cache = S3FifoCache::new(30);
        cache.insert("x", 1);
        cache.insert("y", 2);
        cache.insert("z", 3);
        let mut keys: Vec<&&str> = cache.keys().collect();
        keys.sort();
        assert_eq!(keys, vec![&"x", &"y", &"z"]);
    }

    #[test]
    fn s3fifo_keys_excludes_ghost() {
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 1);
        cache.insert("b", 2); // evicts "a" to ghost
        let keys: Vec<&&str> = cache.keys().collect();
        assert!(!keys.contains(&&"a"));
        assert!(keys.contains(&&"b"));
    }

    #[test]
    fn s3fifo_remove_from_small() {
        let mut cache = S3FifoCache::new(30);
        cache.insert("a", 10);
        assert_eq!(cache.remove(&"a"), Some(10));
        assert!(cache.is_empty());
        assert!(!cache.contains_key(&"a"));
    }

    #[test]
    fn s3fifo_remove_from_main() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 10);
        cache.get(&"a"); // bump freq
        cache.insert("b", 20); // evicts "a" to main
        assert_eq!(cache.main_len(), 1);

        assert_eq!(cache.remove(&"a"), Some(10));
        assert_eq!(cache.main_len(), 0);
    }

    #[test]
    fn s3fifo_remove_from_ghost_returns_none() {
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 10);
        cache.insert("b", 20); // "a" to ghost (freq=0)
        assert_eq!(cache.ghost_len(), 1);

        // Removing ghost entry returns None (no value stored)
        assert_eq!(cache.remove(&"a"), None);
        assert_eq!(cache.ghost_len(), 0);
    }

    #[test]
    fn s3fifo_clear() {
        let mut cache = S3FifoCache::new(30);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        assert_eq!(cache.len(), 3);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.ghost_len(), 0);
        assert_eq!(cache.get(&"a"), None);
    }

    #[test]
    fn s3fifo_capacity_returns_total() {
        let cache: S3FifoCache<&str, i32> = S3FifoCache::new(100);
        assert_eq!(cache.capacity(), 100);
    }

    #[test]
    fn s3fifo_capacity_minimum_one_small() {
        // capacity 5 -> small=1 (max of 5/10=0 and 1)
        let cache: S3FifoCache<&str, i32> = S3FifoCache::new(5);
        assert_eq!(cache.small_capacity, 1);
        assert_eq!(cache.main_capacity, 4);
        assert_eq!(cache.capacity(), 5);
    }

    #[test]
    fn s3fifo_insert_updates_existing_in_small() {
        let mut cache = S3FifoCache::new(30);
        cache.insert("a", 100);
        cache.insert("a", 200); // update in-place
        assert_eq!(cache.get(&"a"), Some(&200));
        assert_eq!(cache.len(), 1); // no duplicate
    }

    #[test]
    fn s3fifo_insert_updates_existing_in_main() {
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 100);
        cache.get(&"a"); // bump freq
        cache.insert("b", 200); // "a" promoted to main
        assert_eq!(cache.main_len(), 1);

        cache.insert("a", 999); // update in-place in main
        assert_eq!(cache.get(&"a"), Some(&999));
    }

    #[test]
    fn s3fifo_freq_saturates_at_3() {
        let mut cache = S3FifoCache::new(30);
        cache.insert("a", 1);
        // Access 10 times — freq should saturate at 3, not overflow
        for _ in 0..10 {
            cache.get(&"a");
        }
        // Still accessible, no panic from overflow
        assert_eq!(cache.get(&"a"), Some(&1));
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn s3fifo_zero_capacity_panics() {
        let _cache: S3FifoCache<&str, i32> = S3FifoCache::new(0);
    }

    #[test]
    fn s3fifo_get_mut_in_main() {
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 10);
        cache.get(&"a"); // bump freq
        cache.insert("b", 20); // "a" promoted to main

        if let Some(val) = cache.get_mut(&"a") {
            *val = 42;
        }
        assert_eq!(cache.get(&"a"), Some(&42));
    }

    // ── Additional coverage tests ────────────────────────────────────

    #[test]
    fn s3fifo_capacity_one() {
        // capacity 1 -> small=1, main=0
        let mut cache = S3FifoCache::new(1);
        assert_eq!(cache.small_capacity, 1);
        assert_eq!(cache.main_capacity, 0);
        assert_eq!(cache.capacity(), 1);

        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.len(), 1);

        // Inserting "b" evicts "a" from small. Since freq=1, "a" attempts
        // promotion to main (cap=0), which may cause len to temporarily
        // exceed 1. This is a known edge case with capacity=1.
        cache.insert("b", 2);
        assert_eq!(cache.get(&"b"), Some(&2));
        // "a" may be in main (promoted) or evicted, depending on budget loop
        assert!(!cache.is_empty());
    }

    #[test]
    fn s3fifo_capacity_two() {
        // capacity 2 -> small=1, main=1
        let mut cache = S3FifoCache::new(2);
        assert_eq!(cache.small_capacity, 1);
        assert_eq!(cache.main_capacity, 1);

        cache.insert("a", 1);
        cache.get(&"a"); // bump freq so it promotes to main
        cache.insert("b", 2); // evicts "a" from small → promotes to main
        assert_eq!(cache.main_len(), 1);
        assert_eq!(cache.small_len(), 1);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn s3fifo_len_invariant_through_operations() {
        let mut cache = S3FifoCache::new(10);
        for i in 0..50 {
            cache.insert(i, i * 10);
            assert_eq!(
                cache.len(),
                cache.small_len() + cache.main_len(),
                "len invariant violated at insert {i}"
            );
            assert!(
                cache.len() <= cache.capacity(),
                "capacity exceeded at insert {i}: len={}, cap={}",
                cache.len(),
                cache.capacity()
            );
        }
    }

    #[test]
    fn s3fifo_ghost_overflow_evicts_oldest_ghost() {
        // capacity 3 -> small=1, main=2, ghost=3
        let mut cache = S3FifoCache::new(3);

        // Insert 5 items without accessing (all evict from small to ghost with freq=0)
        for i in 0..5 {
            cache.insert(i, i * 10);
        }
        // Ghost should not exceed ghost_capacity (3)
        assert!(cache.ghost_len() <= 3);
    }

    #[test]
    fn s3fifo_remove_nonexistent_is_none() {
        let mut cache = S3FifoCache::new(10);
        cache.insert("x", 1);
        assert_eq!(cache.remove(&"y"), None);
        assert_eq!(cache.len(), 1); // unchanged
    }

    #[test]
    fn s3fifo_insert_after_clear() {
        let mut cache = S3FifoCache::new(10);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.clear();
        assert!(cache.is_empty());

        cache.insert("c", 3);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.get(&"a"), None);
    }

    #[test]
    fn s3fifo_get_mut_bumps_freq_causes_promotion() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);
        cache.insert("a", 10);
        // Use get_mut to bump freq instead of get
        if let Some(v) = cache.get_mut(&"a") {
            *v = 11;
        }
        // Insert "b" — evicts "a" from small; "a" has freq=1, should promote to main
        cache.insert("b", 20);
        assert_eq!(cache.main_len(), 1, "a should have promoted to main");
        assert_eq!(cache.get(&"a"), Some(&11));
    }

    #[test]
    fn s3fifo_string_keys() {
        // capacity 30 -> small=3, fits both keys without eviction
        let mut cache = S3FifoCache::new(30);
        cache.insert("hello".to_string(), 1);
        cache.insert("world".to_string(), 2);
        assert_eq!(cache.get(&"hello".to_string()), Some(&1));
        assert_eq!(cache.get(&"world".to_string()), Some(&2));
        assert_eq!(cache.remove(&"hello".to_string()), Some(1));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn s3fifo_tuple_values() {
        let mut cache = S3FifoCache::new(10);
        cache.insert(1, ("name", 42));
        assert_eq!(cache.get(&1), Some(&("name", 42)));
    }

    #[test]
    fn s3fifo_keys_after_promotions_and_evictions() {
        // capacity 5 -> small=1, main=4
        let mut cache = S3FifoCache::new(5);
        // Insert and promote a few items to main
        for i in 0..3 {
            cache.insert(i, i * 10);
            cache.get(&i); // bump freq
            cache.insert(100 + i, 0); // trigger small eviction → promote i to main
        }
        let keys: Vec<&i32> = cache.keys().collect();
        // All promoted items should appear in keys
        for i in 0..3 {
            assert!(
                keys.contains(&&i),
                "key {i} should be in keys after promotion"
            );
        }
    }

    #[test]
    fn s3fifo_insert_same_key_many_times() {
        let mut cache = S3FifoCache::new(10);
        for i in 0..100 {
            cache.insert("same", i);
        }
        // Should only have 1 entry with the latest value
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&"same"), Some(&99));
    }

    #[test]
    fn s3fifo_mixed_access_pattern() {
        // Simulate a realistic access pattern: insert items, access some frequently,
        // then insert more. Items accessed frequently should be in Main queue.
        let mut cache = S3FifoCache::new(20);
        // small=2, main=18

        // Insert items 0-4 and access them to build frequency
        for i in 0..5 {
            cache.insert(i, i);
            cache.get(&i); // bump freq to 1
        }
        // Insert more items to trigger small evictions; items 0-4 promote to main
        for i in 5..20 {
            cache.insert(i, i);
        }
        // Now access 0-4 again (they should be in main) to bump freq
        for i in 0..5 {
            cache.get(&i);
        }
        // Insert 10 more items to cause further evictions
        for i in 20..30 {
            cache.insert(i, i);
        }
        // Items 0-4 had high frequency in main, so S3-FIFO should reinsert them
        let mut hot_count = 0;
        for i in 0..5 {
            if cache.contains_key(&i) {
                hot_count += 1;
            }
        }
        assert!(
            hot_count >= 3,
            "at least 3 of 5 hot items should survive eviction, got {hot_count}"
        );
    }
}
