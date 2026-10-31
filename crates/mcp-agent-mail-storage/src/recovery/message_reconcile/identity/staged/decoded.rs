//! Reuse a decoded private index only after reading byte-identical input again.
//!
//! This caches serialized index interpretation, never message IDs, object bytes,
//! HEAD observations, budgets, or a successful repair verdict. All authority
//! checks still run against the caller's current repository and current message.
//! The native `Index` is neither `Send` nor `Sync`; only owned fields cross threads.

use std::borrow::Cow;
use std::io::Write;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use git2::{ErrorCode, Index, ObjectFormat, Oid, Repository};

use super::{CanonicalScanBudget, invalid};

const MAX_CACHED_RETAINED_BYTES: usize = 32 * 1024 * 1024;
// The encoded input and decoded fields share one unchanged retention bound.
// A large index can fit when its paths already live in the retained v2/v3
// bytes; duplicating those paths needlessly excluded production-sized inputs.
const MAX_CACHED_INPUT_BYTES: usize = MAX_CACHED_RETAINED_BYTES;

#[derive(Clone)]
pub(super) struct Entry<'a> {
    pub(super) path: Cow<'a, [u8]>,
    pub(super) id: Oid,
    pub(super) mode: u32,
    pub(super) flags: u16,
}

impl From<git2::IndexEntry> for Entry<'_> {
    fn from(entry: git2::IndexEntry) -> Self {
        Self {
            path: Cow::Owned(entry.path),
            id: entry.id,
            mode: entry.mode,
            flags: entry.flags,
        }
    }
}

enum CachedPath {
    Encoded(Range<usize>),
    Owned(Vec<u8>),
}

struct CachedEntry {
    path: CachedPath,
    id: Oid,
    mode: u32,
    flags: u16,
}

impl CachedEntry {
    fn path<'a>(&'a self, encoded: &'a [u8]) -> &'a [u8] {
        match &self.path {
            CachedPath::Encoded(range) => &encoded[range.clone()],
            CachedPath::Owned(path) => path,
        }
    }
}

/// Locate uncompressed v2/v3 names only after libgit2 has decoded the index.
/// This does not interpret index identity, modes, stages or extensions. Each
/// candidate slice must equal the native name before it can replace that name's
/// owned allocation. Unsupported or unproved layouts retain native paths.
struct EncodedPaths {
    next: Option<usize>,
}

impl EncodedPaths {
    fn new(encoded: &[u8]) -> Self {
        let version = encoded
            .get(4..8)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_be_bytes);
        Self {
            next: matches!(version, Some(2 | 3)).then_some(12),
        }
    }

    fn take(&mut self, encoded: &[u8], expected: &[u8], oid_bytes: usize) -> Option<Range<usize>> {
        // Remove the cursor first. Any failed bound/equality check disables
        // subsequent ranges, instead of guessing where the next entry starts.
        let start = self.next.take()?;
        if !matches!(oid_bytes, 20 | 32) {
            return None;
        }
        let flags_start = start.checked_add(40)?.checked_add(oid_bytes)?;
        let flags_end = flags_start.checked_add(2)?;
        let flags = u16::from_be_bytes(encoded.get(flags_start..flags_end)?.try_into().ok()?);
        let path_start = flags_end.checked_add(if flags & 0x4000 == 0 { 0 } else { 2 })?;
        let path_end = path_start.checked_add(expected.len())?;
        if encoded.get(path_start..path_end)? != expected || *encoded.get(path_end)? != 0 {
            return None;
        }
        let used = path_end.checked_add(1)?.checked_sub(start)?;
        let padded = used.checked_add(7)? & !7;
        let next = start.checked_add(padded)?;
        // Padding belongs to this entry. It must be in bounds and zero; the
        // native decoder remains the authority for all following extensions.
        if encoded.get(path_end..next)?.iter().any(|byte| *byte != 0) {
            return None;
        }
        self.next = Some(next);
        Some(path_start..path_end)
    }
}

struct Decoded {
    encoded: Box<[u8]>,
    format: ObjectFormat,
    entries: Box<[CachedEntry]>,
}

impl Decoded {
    fn find_prefix(&self, prefix: &str) -> Option<usize> {
        let prefix = prefix.as_bytes();
        let start = self
            .entries
            .partition_point(|entry| entry.path(&self.encoded) < prefix);
        self.entries
            .get(start)
            .filter(|entry| entry.path(&self.encoded).starts_with(prefix))
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
        encoded: &[u8],
        budget: &CanonicalScanBudget<'_>,
    ) -> crate::Result<Option<Box<[CachedEntry]>>> {
        if encoded.len() > self.max_input_bytes {
            return Ok(None);
        }
        let Some(mut retained) = index
            .len()
            .checked_mul(std::mem::size_of::<CachedEntry>())
            .and_then(|bytes| bytes.checked_add(encoded.len()))
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Decoded>()))
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .filter(|bytes| *bytes <= self.max_retained_bytes)
        else {
            return Ok(None);
        };
        let mut entries: Vec<CachedEntry> = Vec::with_capacity(index.len());
        let mut paths = EncodedPaths::new(encoded);
        for entry in index.iter() {
            budget.checkpoint("staged index cache entry")?;
            if entries
                .last()
                .is_some_and(|previous| previous.path(encoded) > entry.path.as_slice())
            {
                // The cache implements bytewise prefix lookup. Unexpected
                // native ordering must retain native lookup, never be sorted
                // into a different interpretation of conflicting stages.
                return Ok(None);
            }
            let path =
                if let Some(range) = paths.take(encoded, &entry.path, entry.id.as_bytes().len()) {
                    CachedPath::Encoded(range)
                } else {
                    let Some(next) = retained
                        .checked_add(entry.path.capacity())
                        .filter(|bytes| *bytes <= self.max_retained_bytes)
                    else {
                        // Capacity is not an admission limit. Keep the existing
                        // native observation when even owned fallback cannot fit.
                        return Ok(None);
                    };
                    retained = next;
                    CachedPath::Owned(entry.path)
                };
            entries.push(CachedEntry {
                path,
                id: entry.id,
                mode: entry.mode,
                flags: entry.flags,
            });
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

    pub(super) fn get(&self, position: usize) -> Option<Entry<'_>> {
        match &self.view {
            View::Cached(decoded) => decoded.entries.get(position).map(|entry| Entry {
                path: Cow::Borrowed(entry.path(&decoded.encoded)),
                id: entry.id,
                mode: entry.mode,
                flags: entry.flags,
            }),
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
    if let Some(entries) = cache.copy_entries(&index, &bytes, budget)? {
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

    fn add_index_entry(index: &mut Index, path: &[u8], id: Oid, flags: u16, extended: u16) {
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags,
                flags_extended: extended,
                path: path.to_vec(),
            })
            .unwrap();
    }

    fn assert_native_entries(observed: &ObservedIndex, native: &Index) {
        assert_eq!(observed.len(), native.len());
        for position in 0..native.len() {
            let expected = native.get(position).unwrap();
            let actual = observed.get(position).unwrap();
            let actual_path: &[u8] = actual.path.as_ref();
            assert_eq!(
                (actual_path, actual.id, actual.mode, actual.flags),
                (
                    expected.path.as_slice(),
                    expected.id,
                    expected.mode,
                    expected.flags
                )
            );
        }
    }

    #[test]
    fn compact_paths_reuse_native_decoding_within_the_same_retention_limit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut native = repo.index().unwrap();
        let id = repo.blob(b"one real object for the index fixture").unwrap();
        for number in 0..128 {
            let path = format!(
                "projects/compact/messages/2026/10/{number:03}_{}.md",
                "x".repeat(205)
            );
            add_index_entry(&mut native, path.as_bytes(), id, 0, 0);
        }
        native.set_version(2).unwrap();
        native.write().unwrap();
        let bytes = std::fs::read(repo.path().join("index")).unwrap();
        // This fixed limit is identical in the baseline and candidate. Native
        // paths duplicated beside the encoded bytes exceed it; verified ranges
        // fit. It is a small real-index causal test, not a large-mailbox timing
        // result or permission to raise the production retention allowance.
        let cache = Cache {
            max_input_bytes: 64 * 1024,
            max_retained_bytes: 64 * 1024,
            ..Cache::new()
        };
        let budget = CanonicalScanBudget::default();
        let first = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        let second = open_with_cache(&repo, bytes.clone(), count(&bytes), &budget, &cache).unwrap();
        assert!(matches!(&first.view, View::Cached(_)));
        assert!(matches!(&second.view, View::Cached(_)));
        assert_eq!(cache.decodes.load(Ordering::Relaxed), 1);
        assert_native_entries(&first, &native);
        assert_native_entries(&second, &native);
        for prefix in ["", "projects/compact/", "projects/foreign/", "zz"] {
            assert_eq!(
                second.find_prefix(prefix).unwrap(),
                native.find_prefix(prefix).ok()
            );
        }
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), bytes);
    }

    #[test]
    fn native_versions_extended_flags_and_merge_stages_keep_their_exact_interpretation() {
        for version in [2, 3, 4] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let mut native = repo.index().unwrap();
            let id = repo.blob(b"native path layout control").unwrap();
            for path in [b"Alpha".as_slice(), b"alpha", b"folder/entry"] {
                add_index_entry(&mut native, path, id, 0, 0);
            }
            if version == 3 {
                // CE_EXTENDED and CE_SKIP_WORKTREE force a true v3 index;
                // otherwise libgit2 may serialize a requested v3 as v2.
                add_index_entry(&mut native, b"extended/file", id, 0x4000, 0x4000);
            }
            for stage in 1..=3 {
                add_index_entry(&mut native, b"merged/conflict", id, stage << 12, 0);
            }
            native.set_version(version).unwrap();
            native.write().unwrap();
            let bytes = std::fs::read(repo.path().join("index")).unwrap();
            assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), version);
            let native = Index::open_ext(&repo.path().join("index"), repo.object_format()).unwrap();
            let cache = Cache::new();
            let observed = open_with_cache(
                &repo,
                bytes.clone(),
                count(&bytes),
                &CanonicalScanBudget::default(),
                &cache,
            )
            .unwrap();
            let View::Cached(decoded) = &observed.view else {
                panic!("small supported native indexes must remain cacheable");
            };
            assert_native_entries(&observed, &native);
            for entry in decoded.entries.iter() {
                assert_eq!(
                    matches!(entry.path, CachedPath::Encoded(_)),
                    version != 4,
                    "v2/v3 ranges and v4 owned fallback must use their proven layouts"
                );
            }
            for prefix in ["A", "alpha", "folder/", "merged/", "missing"] {
                assert_eq!(
                    observed.find_prefix(prefix).unwrap(),
                    native.find_prefix(prefix).ok()
                );
            }
            // On an exact name with stages libgit2 stops wherever its binary
            // search lands, so only the path compares, and that is all
            // reject_ancestor_entries reads. The decoded view stays on stage 1.
            let first_stage = observed.find_prefix("merged/conflict").unwrap().unwrap();
            assert_eq!(first_stage, native.len() - 3);
            let found = observed.get(first_stage).unwrap();
            let found_path: &[u8] = found.path.as_ref();
            let any_stage = native.find_prefix("merged/conflict").unwrap();
            assert_eq!(found_path, native.get(any_stage).unwrap().path.as_slice());
            let warm = open_with_cache(
                &repo,
                bytes.clone(),
                count(&bytes),
                &CanonicalScanBudget::default(),
                &cache,
            )
            .unwrap();
            assert_eq!(cache.decodes.load(Ordering::Relaxed), 1);
            assert_native_entries(&warm, &native);
            assert!(matches!(warm.get(0).unwrap().path, Cow::Borrowed(_)));
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), bytes);
        }
    }

    fn encoded_path_fixture(
        version: u32,
        oid_bytes: usize,
        extended: bool,
    ) -> (Vec<u8>, Vec<Vec<u8>>) {
        let names = [b"a".to_vec(), b"aligned/path".to_vec(), vec![b'x'; 4096]];
        let mut bytes = b"DIRC".to_vec();
        bytes.extend(version.to_be_bytes());
        bytes.extend(3_u32.to_be_bytes());
        for name in &names {
            let start = bytes.len();
            bytes.resize(start + 40 + oid_bytes, 0);
            bytes.extend(if extended { 0x4000_u16 } else { 0 }.to_be_bytes());
            if extended {
                bytes.extend(0x4000_u16.to_be_bytes());
            }
            bytes.extend(name);
            bytes.push(0);
            let size = (bytes.len() - start + 7) & !7;
            bytes.resize(start + size, 0);
        }
        (bytes, names.to_vec())
    }

    #[test]
    fn encoded_ranges_follow_padding_extensions_and_both_object_widths() {
        for version in [2, 3] {
            for oid_bytes in [20, 32] {
                for extended in [false, true] {
                    let (bytes, names) = encoded_path_fixture(version, oid_bytes, extended);
                    let mut paths = EncodedPaths::new(&bytes);
                    for name in &names {
                        let range = paths.take(&bytes, name, oid_bytes).unwrap();
                        assert_eq!(&bytes[range], name);
                    }
                    assert_eq!(paths.next, Some(bytes.len()));
                    assert!(paths.take(&bytes, b"unexpected", oid_bytes).is_none());
                    assert!(paths.next.is_none());
                }
            }
        }
    }

    #[test]
    fn unproved_encoded_ranges_disable_offsets_without_guessing() {
        let (bytes, names) = encoded_path_fixture(2, 20, false);
        for end in 0..12 + 64 {
            let truncated = &bytes[..end];
            let mut paths = EncodedPaths::new(truncated);
            assert!(paths.take(truncated, &names[0], 20).is_none());
            assert!(paths.next.is_none());
        }
        for invalid_width in [0, 19, 21, 31, 33, usize::MAX] {
            let mut paths = EncodedPaths::new(&bytes);
            assert!(paths.take(&bytes, &names[0], invalid_width).is_none());
            assert!(paths.next.is_none());
        }
        let mut mismatched = EncodedPaths::new(&bytes);
        assert!(mismatched.take(&bytes, b"b", 20).is_none());
        assert!(mismatched.take(&bytes, &names[0], 20).is_none());
        let mut corrupt = bytes.clone();
        corrupt[12 + 63] = 1; // First entry's terminating NUL/padding.
        let mut paths = EncodedPaths::new(&corrupt);
        assert!(paths.take(&corrupt, &names[0], 20).is_none());
        let mut overflow = EncodedPaths {
            next: Some(usize::MAX),
        };
        assert!(overflow.take(&bytes, &names[0], 20).is_none());
        for version in [0, 1, 4, u32::MAX] {
            let mut other = bytes.clone();
            other[4..8].copy_from_slice(&version.to_be_bytes());
            let mut paths = EncodedPaths::new(&other);
            assert!(paths.take(&other, &names[0], 20).is_none());
        }
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
                (cached.path.as_ref(), cached.id, cached.mode, cached.flags),
                (entry.path.as_slice(), entry.id, entry.mode, entry.flags)
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
