//! Process-global string interning for frequently-reused identifiers.
//!
//! Agent names, project slugs, policy strings, and thread IDs are duplicated
//! many times across caches, query results, and JSON responses.  `InternedStr`
//! is a newtype over `Arc<str>` that deduplicates these strings in a global
//! intern table. The table bounds both retained entries and UTF-8 payload bytes;
//! cache misses cannot retain every identifier seen over the daemon's lifetime.
//! Eviction releases only the table's ownership, so existing handles stay valid.
//!
//! **Clone cost:** one atomic ref-count increment (vs. heap alloc + memcpy for String).
//!
//! **Interning cost:** a read lock for a retained value, and a write lock on a
//! miss. FIFO admission and eviction are O(1) amortized. Values larger than the
//! per-string limit are returned without retaining or locking the table.

#![forbid(unsafe_code)]

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// InternedStr
// ---------------------------------------------------------------------------

/// A reference-counted, interned string.
///
/// Cheap to clone (atomic ref-count bump), with a pointer-equality fast path and
/// content-based equality/hash semantics. Equal values can have distinct
/// allocations after eviction or when too large to retain.
#[derive(Clone)]
pub struct InternedStr(Arc<str>);

impl InternedStr {
    /// Intern a string slice, returning a shared handle.
    ///
    /// If the string is still retained, the existing `Arc` is reused.
    #[inline]
    #[must_use]
    pub fn new(s: &str) -> Self {
        intern(s)
    }

    /// Create from an owned `String`, interning it.
    #[inline]
    #[must_use]
    pub fn from_string(s: &str) -> Self {
        intern(s)
    }

    /// Return the underlying `Arc<str>` (useful for embedding in structs
    /// that already store `Arc<str>`).
    #[inline]
    #[must_use]
    pub fn into_arc(self) -> Arc<str> {
        self.0
    }

    /// Whether both handles share an allocation. This can be false for equal
    /// strings after eviction; use `PartialEq` for the content-based comparison.
    #[inline]
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

// --- Transparent &str access ---

impl Deref for InternedStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InternedStr {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for InternedStr {
    #[inline]
    fn borrow(&self) -> &str {
        &self.0
    }
}

// --- Equality / Hash (by string content, not pointer) ---

impl PartialEq for InternedStr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // Fast path: same Arc → same string.
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl Eq for InternedStr {}

impl PartialEq<str> for InternedStr {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for InternedStr {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for InternedStr {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

impl Hash for InternedStr {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (*self.0).hash(state);
    }
}

impl PartialOrd for InternedStr {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InternedStr {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self.0).cmp(&*other.0)
    }
}

// --- Display / Debug ---

impl fmt::Display for InternedStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for InternedStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InternedStr({:?})", &*self.0)
    }
}

// --- From conversions ---

impl From<&str> for InternedStr {
    #[inline]
    fn from(s: &str) -> Self {
        intern(s)
    }
}

impl From<String> for InternedStr {
    #[inline]
    fn from(s: String) -> Self {
        intern(&s)
    }
}

impl From<Arc<str>> for InternedStr {
    #[inline]
    fn from(arc: Arc<str>) -> Self {
        // Reuse a retained allocation when one is available.
        intern(&arc)
    }
}

impl From<InternedStr> for String {
    #[inline]
    fn from(s: InternedStr) -> Self {
        s.0.to_string()
    }
}

// --- Serde ---

impl Serialize for InternedStr {
    #[inline]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InternedStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(intern(&s))
    }
}

// ---------------------------------------------------------------------------
// Global interner
// ---------------------------------------------------------------------------

// Entry and byte limits bound both the identifier payload and the set/FIFO
// bookkeeping. Caller-owned handles are separate from this retained cache.
const INTERN_MAX_ENTRIES: usize = 16_384;
const INTERN_MAX_BYTES: usize = 8 * 1024 * 1024;
const INTERN_MAX_STRING_BYTES: usize = 16 * 1024;

struct InternTable {
    entries: HashSet<Arc<str>>,
    order: VecDeque<Arc<str>>,
    retained_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl InternTable {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashSet::with_capacity(max_entries.min(256)),
            order: VecDeque::with_capacity(max_entries.min(256)),
            retained_bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    fn intern(&mut self, s: &str) -> InternedStr {
        // An uncacheable value must not evict useful retained identifiers.
        if self.max_entries == 0 || s.len() > self.max_bytes || s.len() > INTERN_MAX_STRING_BYTES {
            return InternedStr(Arc::from(s));
        }
        if let Some(existing) = self.entries.get(s) {
            return InternedStr(Arc::clone(existing));
        }

        while self.entries.len() >= self.max_entries
            || s.len() > self.max_bytes.saturating_sub(self.retained_bytes)
        {
            let Some(oldest) = self.order.pop_front() else {
                // Keep admission bounded even if bookkeeping is unavailable.
                return InternedStr(Arc::from(s));
            };
            if self.entries.remove(oldest.as_ref()) {
                self.retained_bytes -= oldest.len();
            }
        }

        let arc: Arc<str> = Arc::from(s);
        self.retained_bytes += arc.len();
        self.entries.insert(Arc::clone(&arc));
        self.order.push_back(Arc::clone(&arc));
        InternedStr(arc)
    }
}

/// The interner is a leaf lock, never held while acquiring another lock.
static INTERNER: LazyLock<RwLock<InternTable>> =
    LazyLock::new(|| RwLock::new(InternTable::new(INTERN_MAX_ENTRIES, INTERN_MAX_BYTES)));

/// Intern a string, returning a shared `InternedStr`.
///
/// Thread-safe. Retained hits use only a read lock. New values obey finite
/// entry/byte budgets; eviction never invalidates an outstanding handle.
#[must_use]
pub fn intern(s: &str) -> InternedStr {
    intern_in(&INTERNER, s)
}

fn intern_in(interner: &RwLock<InternTable>, s: &str) -> InternedStr {
    // Do not hash a giant cache-miss key while holding a process-global lock.
    if s.len() > INTERN_MAX_STRING_BYTES {
        return InternedStr(Arc::from(s));
    }
    // Fast path: try to get the existing Arc under a read lock.
    {
        let table = interner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = table.entries.get(s) {
            return InternedStr(Arc::clone(existing));
        }
    }

    // Re-check under the write lock before bounded admission, so concurrent
    // misses for the same value cannot create duplicate retained entries.
    let mut table = interner
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let handle = table.intern(s);
    drop(table);
    handle
}

/// Number of unique strings currently retained by the bounded intern table.
/// This is not a lifetime distinct-value count and can decrease on eviction.
#[must_use]
pub fn intern_count() -> usize {
    INTERNER
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .len()
}

/// Pre-intern a set of known strings (e.g., policy values).
///
/// Call once at startup to avoid first-request admission work. Preloading obeys
/// the same limits as normal lookups; it cannot pin an unbounded set of values.
pub fn pre_intern(strings: &[&str]) {
    for &s in strings {
        if s.len() <= INTERN_MAX_STRING_BYTES {
            let _ = intern(s);
        }
    }
}

// ---------------------------------------------------------------------------
// Well-known interned constants
// ---------------------------------------------------------------------------

/// Pre-intern all well-known policy strings.  Call once at startup.
pub fn pre_intern_policies() {
    pre_intern(&[
        "auto",
        "inline",
        "file", // attachments_policy
        "open",
        "contacts_only",
        "block_all", // contact_policy
    ]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_deduplicates() {
        let a = intern("hello");
        let b = intern("hello");
        assert!(Arc::ptr_eq(&a.0, &b.0), "same string should share Arc");
        assert_eq!(a, b);
    }

    #[test]
    fn intern_different_strings() {
        let a = intern("alpha");
        let b = intern("beta");
        assert!(!Arc::ptr_eq(&a.0, &b.0));
        assert_ne!(a, b);
    }

    #[test]
    fn from_string_interns() {
        let a = InternedStr::from_string("world");
        let b = InternedStr::new("world");
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn deref_to_str() {
        let s = intern("test");
        let r: &str = &s;
        assert_eq!(r, "test");
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn eq_with_str() {
        let s = intern("cmp");
        assert_eq!(s, "cmp");
        assert_eq!(s, *"cmp");
        assert_eq!(s, "cmp".to_string());
    }

    #[test]
    fn hash_consistency() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(intern("key"), 42);
        assert_eq!(map.get("key"), Some(&42));
    }

    #[test]
    fn ordering() {
        let a = intern("alpha");
        let b = intern("beta");
        assert!(a < b);
    }

    #[test]
    fn serde_roundtrip() {
        let original = intern("serde_test");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"serde_test\"");
        let deserialized: InternedStr = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, original);
        assert!(deserialized.ptr_eq(&original));
    }

    #[test]
    fn pre_intern_policies_works() {
        pre_intern_policies();
        let a = intern("auto");
        let b = intern("auto");
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn retained_count_increases_until_capacity() {
        let table = RwLock::new(InternTable::new(2, 64));
        let _ = intern_in(&table, "first");
        assert_eq!(table.read().unwrap().entries.len(), 1);
        let _ = intern_in(&table, "second");
        assert_eq!(table.read().unwrap().entries.len(), 2);
        let _ = intern_in(&table, "third");
        assert_eq!(table.read().unwrap().entries.len(), 2);
    }

    #[test]
    fn display_and_debug() {
        let s = intern("display");
        assert_eq!(format!("{s}"), "display");
        assert_eq!(format!("{s:?}"), "InternedStr(\"display\")");
    }

    #[test]
    fn into_string() {
        let s = intern("convert");
        let owned: String = s.into();
        assert_eq!(owned, "convert");
    }

    #[test]
    fn clone_is_cheap() {
        let a = intern("cheap_clone");
        let b = a.clone();
        assert!(a.ptr_eq(&b));
    }

    // ── br-3h13: Additional intern.rs test coverage ──────────────────

    #[test]
    fn into_arc_returns_arc() {
        let s = intern("arc_test");
        let arc = s.clone().into_arc();
        assert_eq!(&*arc, "arc_test");
        // The returned Arc should be the same as the interned one
        assert!(Arc::ptr_eq(&arc, &s.0));
    }

    #[test]
    fn from_arc_str_re_interns() {
        let original = intern("from_arc");
        let arc: Arc<str> = Arc::from("from_arc");
        let from_arc = InternedStr::from(arc);
        // Should deduplicate via re-interning
        assert!(from_arc.ptr_eq(&original));
    }

    #[test]
    fn from_string_conversion() {
        let s = String::from("from_owned");
        let interned = InternedStr::from(s);
        assert_eq!(&*interned, "from_owned");
    }

    #[test]
    fn from_str_conversion() {
        let interned: InternedStr = "from_ref".into();
        assert_eq!(&*interned, "from_ref");
    }

    #[test]
    fn as_ref_str() {
        let s = intern("as_ref_test");
        let r: &str = s.as_ref();
        assert_eq!(r, "as_ref_test");
    }

    #[test]
    fn borrow_str() {
        use std::borrow::Borrow;
        let s = intern("borrow_test");
        let r: &str = s.borrow();
        assert_eq!(r, "borrow_test");
    }

    #[test]
    fn empty_string_interning() {
        let a = intern("");
        let b = intern("");
        assert!(a.ptr_eq(&b));
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
    }

    #[test]
    fn unicode_string_interning() {
        let a = intern("日本語テスト");
        let b = intern("日本語テスト");
        assert!(a.ptr_eq(&b));
        assert_eq!(&*a, "日本語テスト");
    }

    #[test]
    fn emoji_string_interning() {
        let a = intern("🦀🔧✅");
        let b = intern("🦀🔧✅");
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn long_string_interning() {
        let long = "x".repeat(10_000);
        let a = intern(&long);
        let b = intern(&long);
        assert!(a.ptr_eq(&b));
        assert_eq!(a.len(), 10_000);
    }

    #[test]
    fn partial_ord_consistent_with_ord() {
        let a = intern("alpha_ord");
        let b = intern("beta_ord");
        assert_eq!(a.partial_cmp(&b), Some(std::cmp::Ordering::Less));
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Less);
    }

    #[test]
    fn eq_with_owned_string() {
        let s = intern("eq_owned");
        let owned = "eq_owned".to_string();
        assert_eq!(s, owned);
    }

    #[test]
    fn eq_with_str_ref() {
        let s = intern("eq_ref");
        assert_eq!(s, "eq_ref");
    }

    #[test]
    fn eq_with_double_ref() {
        let s = intern("eq_double");
        let r: &str = "eq_double";
        assert_eq!(s, r);
    }

    #[test]
    fn ne_with_different_string() {
        let a = intern("ne_a");
        let b = intern("ne_b");
        assert_ne!(a, b);
        assert!(a != "ne_b");
    }

    #[test]
    fn hash_in_hashset() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(intern("hash_set_item"));
        assert!(set.contains(&intern("hash_set_item")));
        assert!(!set.contains(&intern("other_item")));
    }

    #[test]
    fn pre_intern_multiple_values() {
        pre_intern(&["pre_a", "pre_b", "pre_c"]);
        let a = intern("pre_a");
        let b = intern("pre_b");
        let c = intern("pre_c");
        // All should be valid
        assert_eq!(&*a, "pre_a");
        assert_eq!(&*b, "pre_b");
        assert_eq!(&*c, "pre_c");
    }

    #[test]
    fn pre_intern_idempotent() {
        let before = intern("pre_idem");
        pre_intern(&["pre_idem"]);
        let after = intern("pre_idem");
        assert!(before.ptr_eq(&after));
    }

    #[test]
    fn concurrent_interning() {
        use std::thread;
        let handles: Vec<_> = (0..8)
            .map(|i| {
                thread::spawn(move || {
                    let key = format!("concurrent_{i}");
                    let a = intern(&key);
                    let b = intern(&key);
                    assert!(a.ptr_eq(&b));
                    a
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn retained_count_reflects_unique_strings() {
        let table = RwLock::new(InternTable::new(4, 64));
        let _ = intern_in(&table, "one");
        let _ = intern_in(&table, "two");
        // Duplicate insert should not increase the interner size.
        let _ = intern_in(&table, "one");
        let retained = table.read().unwrap();
        assert_eq!(retained.entries.len(), 2);
        assert_eq!(retained.retained_bytes, 6);
    }

    #[test]
    fn eviction_preserves_live_handle_equality_and_releases_cache_ownership() {
        let table = RwLock::new(InternTable::new(2, 128));
        let original = intern_in(&table, "evicted-key");
        let weak = Arc::downgrade(&original.0);
        let _ = intern_in(&table, "second");
        let _ = intern_in(&table, "third");
        let replacement = intern_in(&table, "evicted-key");
        assert!(!original.ptr_eq(&replacement));
        assert_eq!(original, replacement);
        assert_eq!(&*original, "evicted-key");

        // Database caches use these values as hash keys. Eviction must not
        // change lookups by a newly interned equal value or by borrowed text.
        let mut cache = std::collections::HashMap::new();
        cache.insert(original.clone(), 42);
        assert_eq!(cache.get(&replacement), Some(&42));
        assert_eq!(cache.get("evicted-key"), Some(&42));
        drop(cache);
        drop(original);
        assert!(
            weak.upgrade().is_none(),
            "the FIFO must release its Arc too"
        );
        assert_eq!(&*replacement, "evicted-key");
    }

    #[test]
    fn byte_budget_counts_utf8_and_oversized_values_do_not_displace_hits() {
        let table = RwLock::new(InternTable::new(16, 5));
        let unicode = intern_in(&table, "éé");
        let weak = Arc::downgrade(&unicode.0);
        assert_eq!(table.read().unwrap().retained_bytes, 4);
        let ordinary = intern_in(&table, "abc");
        assert_eq!(table.read().unwrap().retained_bytes, 3);
        drop(unicode);
        assert!(weak.upgrade().is_none());

        let oversized = intern_in(&table, "too large");
        let oversized_weak = Arc::downgrade(&oversized.0);
        assert_eq!(&*oversized, "too large");
        drop(oversized);
        assert!(oversized_weak.upgrade().is_none());
        assert!(ordinary.ptr_eq(&intern_in(&table, "abc")));
        let retained = table.read().unwrap();
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.retained_bytes, 3);
    }

    #[test]
    fn zero_capacity_and_empty_values_remain_valid() {
        let disabled = RwLock::new(InternTable::new(0, 32));
        let uncached = intern_in(&disabled, "value");
        let weak = Arc::downgrade(&uncached.0);
        drop(uncached);
        assert!(weak.upgrade().is_none());
        assert!(disabled.read().unwrap().entries.is_empty());

        let empty_only = RwLock::new(InternTable::new(1, 0));
        let empty = intern_in(&empty_only, "");
        assert!(empty.ptr_eq(&intern_in(&empty_only, "")));
        assert_eq!(&*intern_in(&empty_only, "x"), "x");
        let retained = empty_only.read().unwrap();
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.retained_bytes, 0);
    }

    #[test]
    fn concurrent_lookup_misses_keep_retention_bounded() {
        let table = Arc::new(RwLock::new(InternTable::new(32, 256)));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    let mut survivors = Vec::new();
                    for index in 0..512 {
                        let value = format!("miss-{worker}-{index}");
                        let handle = intern_in(&table, &value);
                        assert_eq!(handle.as_ref(), value);
                        if index % 128 == 0 {
                            survivors.push(handle);
                        }
                        let (entries, retained_bytes) = {
                            let retained = table.read().unwrap();
                            (retained.entries.len(), retained.retained_bytes)
                        };
                        assert!(entries <= 32);
                        assert!(retained_bytes <= 256);
                    }
                    survivors
                })
            })
            .collect();
        for worker in workers {
            for survivor in worker.join().unwrap() {
                assert_eq!(survivor, intern_in(&table, &survivor));
            }
        }
    }

    #[test]
    fn public_lookup_and_preload_share_retention_limits() {
        const CHILD: &str = "AM_TEST_BOUNDED_INTERNER";
        const COMPLETED: &str = "bounded public interner verified";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "intern::tests::public_lookup_and_preload_share_retention_limits",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .expect("isolated public interner regression");
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETED));
            return;
        }

        let first = intern("first-miss");
        let first_weak = Arc::downgrade(&first.0);
        drop(first);
        for index in 0..=INTERN_MAX_ENTRIES {
            let _ = intern(&format!("missing-project-{index}"));
        }
        assert!(first_weak.upgrade().is_none());
        assert_eq!(intern_count(), INTERN_MAX_ENTRIES);

        let payload = "x".repeat(INTERN_MAX_STRING_BYTES - 32);
        let preload: Vec<String> = (0..INTERN_MAX_BYTES / payload.len() + 2)
            .map(|index| format!("preload-{index}-{payload}"))
            .collect();
        let preload_refs: Vec<&str> = preload.iter().map(String::as_str).collect();
        pre_intern(&preload_refs);
        let (entries, retained_bytes) = {
            let retained = INTERNER.read().unwrap();
            (retained.entries.len(), retained.retained_bytes)
        };
        assert!(entries < INTERN_MAX_ENTRIES);
        assert!(retained_bytes <= INTERN_MAX_BYTES);
        let hot = intern("retained-hit");
        let oversized = "x".repeat(INTERN_MAX_STRING_BYTES + 1);
        let uncached = intern(&oversized);
        let uncached_weak = Arc::downgrade(&uncached.0);
        pre_intern(&[&oversized]);
        drop(uncached);
        assert!(uncached_weak.upgrade().is_none());
        assert!(hot.ptr_eq(&intern("retained-hit")));
        println!("{COMPLETED}");
    }

    #[test]
    fn serde_roundtrip_unicode() {
        let original = intern("日本語");
        let json = serde_json::to_string(&original).unwrap();
        let back: InternedStr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn display_vs_debug_format() {
        let s = intern("fmt_test");
        // Display is bare string
        assert_eq!(format!("{s}"), "fmt_test");
        // Debug wraps in InternedStr(...)
        assert_eq!(format!("{s:?}"), "InternedStr(\"fmt_test\")");
    }

    #[test]
    fn ptr_eq_different_strings() {
        let a = intern("ptr_a");
        let b = intern("ptr_b");
        assert!(!a.ptr_eq(&b));
    }

    #[test]
    fn ptr_eq_same_string() {
        let a = intern("ptr_same");
        let b = intern("ptr_same");
        assert!(a.ptr_eq(&b));
    }
}
