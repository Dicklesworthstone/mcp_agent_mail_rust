//! Process-global string interning for frequently-reused identifiers.
//!
//! Agent names, project slugs, policy strings, and thread IDs are duplicated
//! many times across caches, query results, and JSON responses.  `InternedStr`
//! is a newtype over `Arc<str>` that deduplicates these strings in a global
//! intern table.
//!
//! **Clone cost:** one atomic ref-count increment (vs. heap alloc + memcpy for String).
//!
//! **Interning cost:** one mutex acquisition on first encounter; subsequent
//! lookups of the same string value return the existing `Arc` (O(1) amortized).

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// InternedStr
// ---------------------------------------------------------------------------

/// A reference-counted, interned string.
///
/// Cheap to clone (atomic ref-count bump), cheap to compare by pointer when
/// both sides came from the same interner, and transparently usable as `&str`.
#[derive(Clone)]
pub struct InternedStr(Arc<str>);

impl InternedStr {
    /// Intern a string slice, returning a shared handle.
    ///
    /// If the string was seen before, the existing `Arc` is reused.
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

    /// Pointer-equality fast path: true when both handles point to the
    /// same interned allocation.  Falls back to byte comparison otherwise.
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
        // Re-intern to ensure deduplication.
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

/// The global intern table.  Uses a plain `Mutex` (not `OrderedMutex`) because
/// the interner is a leaf lock that is never held while acquiring other locks.
static INTERNER: LazyLock<Mutex<HashSet<Arc<str>>>> =
    LazyLock::new(|| Mutex::new(HashSet::with_capacity(256)));

/// Intern a string, returning a shared `InternedStr`.
///
/// Thread-safe.  The mutex is only contended on first encounter of a new
/// string value; subsequent calls for known strings are a hash lookup + Arc clone.
pub fn intern(s: &str) -> InternedStr {
    let mut table = INTERNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Can't use map_or_else: get() borrows immutably, insert() borrows mutably.
    #[allow(clippy::option_if_let_else)]
    let result = if let Some(existing) = table.get(s) {
        InternedStr(Arc::clone(existing))
    } else {
        let arc: Arc<str> = Arc::from(s);
        table.insert(Arc::clone(&arc));
        InternedStr(arc)
    };
    drop(table);
    result
}

/// Number of unique strings currently interned.
#[must_use]
pub fn intern_count() -> usize {
    INTERNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Pre-intern a set of known strings (e.g., policy values).
///
/// Call once at startup to avoid mutex contention on the first real request.
pub fn pre_intern(strings: &[&str]) {
    let mut table = INTERNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for &s in strings {
        if !table.contains(s) {
            table.insert(Arc::from(s));
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
    fn intern_count_increases() {
        let before = intern_count();
        let unique = format!("unique_{before}");
        let _ = intern(&unique);
        assert!(intern_count() > before);
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
        assert!(s == owned);
    }

    #[test]
    fn eq_with_str_ref() {
        let s = intern("eq_ref");
        assert!(s == "eq_ref");
    }

    #[test]
    fn eq_with_double_ref() {
        let s = intern("eq_double");
        let r: &str = "eq_double";
        assert!(s == r);
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
    fn intern_count_reflects_unique_strings() {
        let unique_prefix = format!("count_test_{}", std::process::id());
        let before = intern_count();
        let _ = intern(&format!("{unique_prefix}_1"));
        let _ = intern(&format!("{unique_prefix}_2"));
        let _ = intern(&format!("{unique_prefix}_1")); // duplicate
        assert_eq!(intern_count(), before + 2);
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
