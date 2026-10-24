//! Reuse a decoded private index only after reading byte-identical input again.
//!
//! This caches serialized index interpretation, never message IDs, object bytes,
//! HEAD observations, budgets, or a successful repair verdict. All authority
//! checks still run against the caller's current repository and current message.
//! The native `Index` is neither `Send` nor `Sync`; only owned fields cross threads.

use std::io::Write;
use std::sync::{Arc, Mutex};

use git2::{ErrorCode, Index, ObjectFormat, Oid, Repository};

use super::{CanonicalScanBudget, invalid};

const MAX_CACHED_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHED_RETAINED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) path: Vec<u8>,
    pub(super) id: Oid,
    pub(super) mode: u32,
    pub(super) flags: u16,
}

impl From<git2::IndexEntry> for Entry {
    fn from(entry: git2::IndexEntry) -> Self {
        Self {
            path: entry.path,
            id: entry.id,
            mode: entry.mode,
            flags: entry.flags,
        }
    }
}

struct Decoded {
    encoded: Box<[u8]>,
    format: ObjectFormat,
    entries: Box<[Entry]>,
}

impl Decoded {
    fn find_prefix(&self, prefix: &str) -> Option<usize> {
        let prefix = prefix.as_bytes();
        let start = self
            .entries
            .partition_point(|entry| entry.path.as_slice() < prefix);
        self.entries
            .get(start)
            .filter(|entry| entry.path.starts_with(prefix))
            .map(|_| start)
    }
}

struct Cache {
    // One shared slot, not one retained native index per request/worker thread.
    slot: Mutex<Option<Arc<Decoded>>>,
    max_input_bytes: usize,
    max_retained_bytes: usize,
    #[cfg(test)]
    decodes: std::sync::atomic::AtomicUsize,
}

impl Cache {
    const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            max_input_bytes: MAX_CACHED_INPUT_BYTES,
            max_retained_bytes: MAX_CACHED_RETAINED_BYTES,
            #[cfg(test)]
            decodes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn find(&self, bytes: &[u8], format: ObjectFormat) -> Option<Arc<Decoded>> {
        // Never retain the mutex across the potentially large byte comparison,
        // native decoding, filesystem I/O, or any caller authority check. Busy
        // or poisoned cache state is simply an uncached observation.
        let candidate = self.slot.try_lock().ok()?.as_ref().cloned()?;
        (candidate.format == format && candidate.encoded.as_ref() == bytes).then_some(candidate)
    }

    fn remember(&self, decoded: &Arc<Decoded>) {
        let previous = self
            .slot
            .try_lock()
            .ok()
            .and_then(|mut slot| slot.replace(Arc::clone(decoded)));
        // Releasing a large old entry vector also happens outside the lock.
        drop(previous);
    }

    fn copy_entries(
        &self,
        index: &Index,
        encoded_len: usize,
        budget: &CanonicalScanBudget<'_>,
    ) -> crate::Result<Option<Box<[Entry]>>> {
        if encoded_len > self.max_input_bytes {
            return Ok(None);
        }
        let Some(mut retained) = index
            .len()
            .checked_mul(std::mem::size_of::<Entry>())
            .and_then(|bytes| bytes.checked_add(encoded_len))
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Decoded>()))
            .filter(|bytes| *bytes <= self.max_retained_bytes)
        else {
            return Ok(None);
        };
        let mut entries: Vec<Entry> = Vec::with_capacity(index.len());
        for entry in index.iter() {
            budget.checkpoint("staged index cache entry")?;
            let entry = Entry::from(entry);
            let Some(next) = retained
                .checked_add(entry.path.capacity())
                .filter(|bytes| *bytes <= self.max_retained_bytes)
            else {
                // Cache capacity is not a new recovery admission limit. Use
                // the already-decoded native index when the cache cannot fit.
                return Ok(None);
            };
            retained = next;
            if entries
                .last()
                .is_some_and(|previous| previous.path.as_slice() > entry.path.as_slice())
            {
                // The cache implements bytewise prefix lookup. Unexpected
                // native ordering must retain native lookup, never be sorted
                // into a different interpretation of conflicting stages.
                return Ok(None);
            }
            entries.push(entry);
        }
        Ok(Some(entries.into_boxed_slice()))
    }
}

static CACHE: Cache = Cache::new();

pub(super) struct ObservedIndex {
    view: View,
}

enum View {
    Cached(Arc<Decoded>),
    Native {
        index: Index,
        // Dropped after the native index. This file is never the live index.
        _snapshot: tempfile::NamedTempFile,
    },
}

impl ObservedIndex {
    pub(super) fn len(&self) -> usize {
        match &self.view {
            View::Cached(decoded) => decoded.entries.len(),
            View::Native { index, .. } => index.len(),
        }
    }

    pub(super) fn get(&self, position: usize) -> Option<Entry> {
        match &self.view {
            View::Cached(decoded) => decoded.entries.get(position).cloned(),
            View::Native { index, .. } => index.get(position).map(Entry::from),
        }
    }

    pub(super) fn find_prefix(&self, prefix: &str) -> crate::Result<Option<usize>> {
        match &self.view {
            View::Cached(decoded) => Ok(decoded.find_prefix(prefix)),
            View::Native { index, .. } => match index.find_prefix(prefix) {
                Ok(position) => Ok(Some(position)),
                Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            },
        }
    }
}

pub(super) fn open(
    repo: &Repository,
    bytes: Vec<u8>,
    count: usize,
    budget: &CanonicalScanBudget<'_>,
) -> crate::Result<ObservedIndex> {
    open_with_cache(repo, bytes, count, budget, &CACHE)
}

fn open_with_cache(
    repo: &Repository,
    bytes: Vec<u8>,
    count: usize,
    budget: &CanonicalScanBudget<'_>,
    cache: &Cache,
) -> crate::Result<ObservedIndex> {
    // The caller has already performed a bounded no-follow read, charged all
    // encoded bytes, and validated the format/count header. Neither timestamp
    // equality nor an old successful source read can create a hit here.
    budget.checkpoint("staged index snapshot")?;
    let format = repo.object_format();
    if let Some(decoded) = cache.find(&bytes, format) {
        budget.checkpoint("staged index decode")?;
        if decoded.entries.len() != count {
            return Err(invalid(
                "staged canonical index decoded entry count changed",
            ));
        }
        return Ok(ObservedIndex {
            view: View::Cached(decoded),
        });
    }
    #[cfg(test)]
    cache
        .decodes
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut snapshot = tempfile::Builder::new()
        .prefix(".canonical-identity-index-")
        .tempfile()?;
    snapshot.write_all(&bytes)?;
    budget.checkpoint("staged index decode")?;
    let index = Index::open_ext(snapshot.path(), format)?;
    if index.len() != count {
        return Err(invalid(
            "staged canonical index decoded entry count changed",
        ));
    }
    if let Some(entries) = cache.copy_entries(&index, bytes.len(), budget)? {
        let decoded = Arc::new(Decoded {
            encoded: bytes.into_boxed_slice(),
            format,
            entries,
        });
        cache.remember(&decoded);
        // Neither the native index nor the private file is retained by a hit.
        drop(index);
        drop(snapshot);
        Ok(ObservedIndex {
            view: View::Cached(decoded),
        })
    } else {
        Ok(ObservedIndex {
            view: View::Native {
                index,
                _snapshot: snapshot,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    fn fixture() -> (tempfile::TempDir, Repository, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        for name in [
            "Alpha",
            "alpha",
            "folder-file",
            "folder-neighbor/file",
            "folder/file",
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, name.as_bytes()).unwrap();
        }
        let mut index = repo.index().unwrap();
        for name in [
            "Alpha",
            "alpha",
            "folder-file",
            "folder-neighbor/file",
            "folder/file",
        ] {
            index.add_path(Path::new(name)).unwrap();
        }
        index.write().unwrap();
        let bytes = std::fs::read(repo.path().join("index")).unwrap();
        drop(index);
        (dir, repo, bytes)
    }

    fn count(bytes: &[u8]) -> usize {
        u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize
    }

    #[test]
    fn exact_bytes_reuse_decoding_and_preserve_native_prefix_verdicts() {
        let (_dir, repo, bytes) = fixture();
        let cache = Cache::new();
        let budget = CanonicalScanBudget::default();
        let first = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        let second = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 1);
        let (View::Cached(left), View::Cached(right)) = (&first.view, &second.view) else {
            panic!("small complete indexes must be cacheable");
        };
        assert!(Arc::ptr_eq(left, right));
        let native = Index::open_ext(&repo.path().join("index"), repo.object_format()).unwrap();
        for prefix in [
            "", "A", "Alpha", "alpha", "folder", "folder/", "folder-", "missing",
        ] {
            assert_eq!(
                second.find_prefix(prefix).unwrap(),
                native.find_prefix(prefix).ok(),
                "{prefix}"
            );
        }
        for position in 0..native.len() {
            let entry = native.get(position).unwrap();
            let cached = second.get(position).unwrap();
            assert_eq!(
                (cached.path, cached.id, cached.mode, cached.flags),
                (entry.path, entry.id, entry.mode, entry.flags)
            );
        }
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), bytes);
    }

    #[test]
    fn changed_input_replaces_the_slot_without_changing_existing_observations() {
        let (dir, repo, bytes) = fixture();
        let cache = Cache::new();
        let budget = CanonicalScanBudget::default();
        let first = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        let original = first.get(0).unwrap().id;
        std::fs::write(dir.path().join("Alpha"), b"other").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("Alpha")).unwrap();
        index.write().unwrap();
        let changed = std::fs::read(repo.path().join("index")).unwrap();
        assert_eq!(changed.len(), bytes.len());
        assert_ne!(changed, bytes);
        let next =
            open_with_cache(&repo, changed.clone(), count(&changed), &budget, &cache).unwrap();
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 2);
        assert_ne!(next.get(0).unwrap().id, original);
        assert_eq!(first.get(0).unwrap().id, original);
        assert!(cache.find(&bytes, repo.object_format()).is_none());
        assert!(cache.find(&changed, repo.object_format()).is_some());
    }

    #[test]
    fn changed_corrupt_bytes_never_fall_back_to_the_cached_index() {
        let (_dir, repo, bytes) = fixture();
        let cache = Cache::new();
        let budget = CanonicalScanBudget::default();
        open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        let mut corrupt = bytes.clone();
        corrupt[0] = b'X';
        assert!(open_with_cache(&repo, corrupt, count(&bytes), &budget, &cache).is_err());
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 2);
        let good = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        assert_eq!(good.len(), count(&bytes));
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 2);
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), bytes);
    }

    #[test]
    fn cache_capacity_is_not_a_new_recovery_limit() {
        let (_dir, repo, bytes) = fixture();
        let budget = CanonicalScanBudget::default();
        for (input, retained) in [(0, usize::MAX), (usize::MAX, 0), (usize::MAX, bytes.len())] {
            let cache = Cache {
                max_input_bytes: input,
                max_retained_bytes: retained,
                ..Cache::new()
            };
            for _ in 0..2 {
                let observed =
                    open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
                assert!(matches!(&observed.view, View::Native { .. }));
                assert_eq!(observed.len(), count(&bytes));
                assert!(observed.find_prefix("Alpha").unwrap().is_some());
            }
            assert_eq!(cache.decodes.load(Ordering::Relaxed), 2);
            assert!(cache.find(&bytes, repo.object_format()).is_none());
        }
    }

    #[test]
    fn contended_cache_does_not_block_or_replace_native_observation() {
        let (_dir, repo, bytes) = fixture();
        let cache = Cache::new();
        let held = cache.slot.lock().unwrap();
        let observed = open_with_cache(
            &repo,
            bytes.clone(),
            count(&bytes),
            &CanonicalScanBudget::default(),
            &cache,
        )
        .unwrap();
        assert_eq!(observed.len(), count(&bytes));
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 1);
        assert!(held.is_none());
        drop(held);
        assert!(cache.find(&bytes, repo.object_format()).is_none());
    }

    #[test]
    fn warm_cache_cannot_override_the_callers_declared_entry_count() {
        let (_dir, repo, bytes) = fixture();
        let cache = Cache::new();
        open_with_cache(
            &repo,
            bytes.clone(),
            count(&bytes),
            &CanonicalScanBudget::default(),
            &cache,
        )
        .unwrap();
        assert!(cache.find(&bytes, repo.object_format()).is_some());
        let rejected = open_with_cache(
            &repo,
            bytes.clone(),
            count(&bytes) + 1,
            &CanonicalScanBudget::default(),
            &cache,
        );
        assert!(rejected.is_err());
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 1);
    }
}
