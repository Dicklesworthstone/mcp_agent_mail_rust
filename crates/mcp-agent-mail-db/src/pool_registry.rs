//! Runtime pool ownership, separate from the recovery generation index.
//!
//! The implementation's weak registry answers which pool is live and lets
//! recovery retire every pool shape for an identity. It cannot also own the
//! lifetime of a request-scoped pool: dropping the last request would turn
//! the next ordinary write into a recovery-barrier bootstrap (GH#353).
//!
//! Keep a bounded working set of file-backed pools alive between requests.
//! ALWAYS look up through the implementation first; retained handles are
//! owners, not lookup results. In particular, retaining an old handle cannot
//! resurrect a generation which recovery closed and removed from its index.

pub use crate::pool_impl::*;

use crate::error::DbResult;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock, PoisonError};

// A normal server has a request pool and a smaller maintenance pool. Bound
// idle ownership across mailbox/config churn instead of pinning every engine
// ever opened by a long-lived process. Active, evicted pools remain findable
// through the weak generation index until their last actual borrower drops.
const WRITE_POOL_RESIDENT_CAPACITY: usize = 4;

struct Residency<T> {
    capacity: usize,
    owners: VecDeque<(String, T)>,
}

impl<T> Residency<T> {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            owners: VecDeque::with_capacity(capacity),
        }
    }

    /// Return displaced ownership to the caller: its destructor may close
    /// engine connections, so it MUST run outside the registry mutex.
    #[must_use]
    fn retain(&mut self, identity: String, make_owner: impl FnOnce() -> T) -> Option<T> {
        if let Some(index) = self.owners.iter().position(|(key, _)| key == &identity) {
            let owner = self.owners.remove(index).expect("located resident pool");
            self.owners.push_back(owner);
            return None;
        }
        let displaced = if self.owners.len() == self.capacity {
            self.owners.pop_front().map(|(_, owner)| owner)
        } else {
            None
        };
        self.owners.push_back((identity, make_owner()));
        displaced
    }
}

static WRITE_POOL_RESIDENTS: OnceLock<Mutex<Residency<DbPool>>> = OnceLock::new();

fn retain_runtime_pool(pool: &DbPool) {
    // Anonymous memory databases have caller-owned lifetimes. Retaining one
    // would silently preserve state after its last caller intentionally drops
    // it, and could join otherwise independent in-memory sessions.
    if pool.sqlite_path() == ":memory:" {
        return;
    }
    // This is the implementation's canonical identity + per-engine cache
    // generation, not the caller's URL spelling or just the database path.
    // Different pool shapes/generations must never replace each other's owner.
    let identity = pool.sqlite_identity_key();
    let displaced = {
        let mut residents = WRITE_POOL_RESIDENTS
            .get_or_init(|| Mutex::new(Residency::new(WRITE_POOL_RESIDENT_CAPACITY)))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        residents.retain(identity, || pool.clone())
    };
    drop(displaced);
}

/// Find an already-open live pool without initialization or promotion.
///
/// Touch its resident ownership so a gap between requests is not a cache miss.
/// Recovery's original authority/closed-generation checks remain authoritative.
#[must_use]
pub fn get_cached_pool(config: &DbPoolConfig) -> Option<DbPool> {
    crate::pool_impl::get_cached_pool(config).inspect(retain_runtime_pool)
}

/// Get a live pool and retain bounded process ownership between requests.
///
/// Construction, migrations and recovery run WITHOUT the residency mutex.
/// Callers that can write must still hold the normal write-admission lease;
/// a genuinely cold bootstrap still requires the existing barrier handoff.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let pool = crate::pool_impl::get_or_create_pool(config)?;
    retain_runtime_pool(&pool);
    Ok(pool)
}

/// Create or reuse a runtime pool with the same ownership as the main factory.
pub fn create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let pool = crate::pool_impl::create_pool(config)?;
    retain_runtime_pool(&pool);
    Ok(pool)
}

/// Preserve compatible-memory-pool selection without pinning memory sessions.
pub fn get_or_reuse_compatible_memory_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let pool = crate::pool_impl::get_or_reuse_compatible_memory_pool(config)?;
    retain_runtime_pool(&pool);
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::Residency;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct Resource(Arc<AtomicUsize>);

    impl Resource {
        fn new(live: &Arc<AtomicUsize>) -> Self {
            live.fetch_add(1, Ordering::SeqCst);
            Self(Arc::clone(live))
        }
    }

    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn idle_ownership_is_bounded_and_hot_entries_are_retained() {
        let live = Arc::new(AtomicUsize::new(0));
        let mut cache = Residency::new(2);
        assert!(cache.retain("a".into(), || Resource::new(&live)).is_none());
        assert!(cache.retain("b".into(), || Resource::new(&live)).is_none());
        // A hit only refreshes recency; it must not make a second owner/engine.
        assert!(
            cache
                .retain("a".into(), || panic!("cache hit constructed a pool"))
                .is_none()
        );
        drop(cache.retain("c".into(), || Resource::new(&live)));
        assert_eq!(live.load(Ordering::SeqCst), 2);
        assert_eq!(
            cache
                .owners
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        for index in 0..1_000 {
            drop(cache.retain(format!("mailbox-{index}"), || Resource::new(&live)));
            assert_eq!(live.load(Ordering::SeqCst), 2);
            assert_eq!(cache.owners.len(), 2);
        }
        drop(cache);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eviction_never_closes_a_resource_still_owned_by_a_worker() {
        let live = Arc::new(AtomicUsize::new(0));
        let worker = Arc::new(Resource::new(&live));
        let weak_index = Arc::downgrade(&worker);
        let mut cache = Residency::new(1);
        assert!(
            cache
                .retain("old-generation".into(), || Arc::clone(&worker))
                .is_none()
        );
        drop(cache.retain("new-generation".into(), || Arc::new(Resource::new(&live))));
        assert!(weak_index.upgrade().is_some());
        assert_eq!(live.load(Ordering::SeqCst), 2);
        drop(worker);
        assert!(weak_index.upgrade().is_none());
        assert_eq!(live.load(Ordering::SeqCst), 1);
        drop(cache);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn displaced_resources_are_destroyed_after_unlocking() {
        struct ReentrantDrop(Arc<Mutex<()>>);
        impl Drop for ReentrantDrop {
            fn drop(&mut self) {
                assert!(
                    self.0.try_lock().is_ok(),
                    "engine destructor ran under registry lock"
                );
            }
        }
        let registry_lock = Arc::new(Mutex::new(()));
        let mut cache = Residency::new(1);
        assert!(
            cache
                .retain("first".into(), || ReentrantDrop(Arc::clone(&registry_lock)))
                .is_none()
        );
        let displaced = {
            let _guard = registry_lock.lock().unwrap();
            cache.retain("second".into(), || ReentrantDrop(Arc::clone(&registry_lock)))
        };
        drop(displaced);
    }
}

#[cfg(test)]
#[path = "pool_registry_workload.rs"]
mod workload_tests;
