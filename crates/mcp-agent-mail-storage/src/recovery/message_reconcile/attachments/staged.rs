//! Last-resort attachment bytes from one private staging-index snapshot.
//!
//! The caller has already confined the path to the project's attachments and
//! must verify the returned bytes against authoritative attachment metadata.
//! Index identity alone is not authority for an uncommitted attachment.

use std::io::Write;
use std::path::Path;

use git2::{Index, ObjectType, Oid, Repository};

use super::invalid;

const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_ENTRIES: u32 = 100_000;

/// Lazily pin one index for the entire attachment bundle. Healthy disk/HEAD
/// paths do not open it; an absent index is also remembered for this pass.
#[derive(Default)]
pub(super) struct StagedAttachments {
    opened: bool,
    snapshot: Option<Snapshot>,
}

struct Snapshot {
    index: Index,
    // Drop libgit2's index before releasing its private backing file.
    _file: tempfile::NamedTempFile,
}

impl StagedAttachments {
    pub(super) fn read(
        &mut self,
        repo: &Repository,
        relative: &str,
        remaining_bytes: usize,
    ) -> crate::Result<Option<Vec<u8>>> {
        if !self.opened {
            self.snapshot = Snapshot::open(repo)?;
            self.opened = true;
        }
        match &self.snapshot {
            Some(snapshot) => snapshot.read(repo, relative, remaining_bytes),
            None => Ok(None),
        }
    }
}

impl Snapshot {
    fn open(repo: &Repository) -> crate::Result<Option<Self>> {
        let path = repo.path().join("index");
        if crate::path_existing_prefix_has_symlink(&path)? {
            return Err(invalid("staged attachment index has a symlinked authority"));
        }
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &path,
            MAX_INDEX_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let header = bytes
            .get(..12)
            .ok_or_else(|| invalid("staged attachment index has an incomplete header"))?;
        if &header[..4] != b"DIRC" {
            return Err(invalid("staged attachment index has an invalid signature"));
        }
        let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        if count > MAX_INDEX_ENTRIES {
            return Err(invalid("staged attachment index entry budget exceeded"));
        }
        let mut file = tempfile::Builder::new()
            .prefix(".attachment-reconcile-index-")
            .tempfile()?;
        file.write_all(&bytes)?;
        drop(bytes);
        // Parse the private copy, never refresh or mutate the real index.
        // Corrupt/unsupported formats and unavailable split indexes fail closed.
        let index = Index::open_ext(file.path(), repo.object_format())?;
        if index.len() > MAX_INDEX_ENTRIES as usize {
            return Err(invalid("staged attachment decoded entry budget exceeded"));
        }
        Ok(Some(Self { index, _file: file }))
    }

    fn read(
        &self,
        repo: &Repository,
        relative: &str,
        remaining_bytes: usize,
    ) -> crate::Result<Option<Vec<u8>>> {
        let key = Path::new(relative);
        if (1..=3).any(|stage| self.index.get_path(key, stage).is_some()) {
            return Err(invalid(
                "staged attachment has unresolved merge stages; preserved",
            ));
        }
        let Some(entry) = self.index.get_path(key, 0) else {
            return Ok(None);
        };
        if entry.path != relative.as_bytes() || !matches!(entry.mode, 0o100644 | 0o100755) {
            return Err(invalid(
                "staged attachment is not an exact regular-file entry",
            ));
        }
        // Check the header before materializing a potentially large object.
        let (size, kind) = repo.odb()?.read_header(entry.id)?;
        if kind != ObjectType::Blob || size > remaining_bytes {
            return Err(invalid(
                "staged attachment recovery bundle byte budget exceeded",
            ));
        }
        let blob = repo.find_blob(entry.id)?;
        if blob.content().len() != size
            || Oid::hash_object_ext(ObjectType::Blob, blob.content(), entry.id.object_format())?
                != entry.id
        {
            return Err(invalid(
                "staged attachment content does not match its object identity",
            ));
        }
        Ok(Some(blob.content().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "projects/test/attachments/files/payload.bin";
    const BYTES: &[u8] = b"staged binary\x00\xff";

    fn fixture() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = dir.path().join(KEY);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, BYTES).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(KEY)).unwrap();
        index.write().unwrap();
        std::fs::rename(path, dir.path().join("retained.bin")).unwrap();
        (dir, repo)
    }

    #[test]
    fn snapshot_returns_exact_bytes_without_touching_index_or_worktree() {
        let (dir, repo) = fixture();
        let index_path = repo.path().join("index");
        let before = std::fs::read(&index_path).unwrap();
        let mut staged = StagedAttachments::default();
        assert_eq!(
            staged.read(&repo, KEY, BYTES.len()).unwrap().unwrap(),
            BYTES
        );
        assert_eq!(std::fs::read(index_path).unwrap(), before);
        assert!(!dir.path().join(KEY).exists());
        assert_eq!(
            std::fs::read(dir.path().join("retained.bin")).unwrap(),
            BYTES
        );
        assert!(staged.read(&repo, KEY, BYTES.len() - 1).is_err());
        assert!(
            staged
                .read(&repo, "projects/test/attachments/absent", 0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bundle_keeps_its_original_snapshot_when_real_index_changes() {
        let (dir, repo) = fixture();
        let mut staged = StagedAttachments::default();
        assert_eq!(staged.read(&repo, KEY, 1024).unwrap().unwrap(), BYTES);
        let path = dir.path().join(KEY);
        std::fs::write(&path, b"replacement").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(KEY)).unwrap();
        index.write().unwrap();
        assert_eq!(staged.read(&repo, KEY, 1024).unwrap().unwrap(), BYTES);
        assert_eq!(
            StagedAttachments::default()
                .read(&repo, KEY, 1024)
                .unwrap()
                .unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn absent_index_is_not_corruption_and_is_pinned_for_the_pass() {
        let (_dir, repo) = fixture();
        let index_path = repo.path().join("index");
        let retained = repo.path().join("retained-index");
        std::fs::rename(&index_path, &retained).unwrap();
        let mut staged = StagedAttachments::default();
        assert!(staged.read(&repo, KEY, 1024).unwrap().is_none());
        std::fs::rename(&retained, &index_path).unwrap();
        assert!(staged.read(&repo, KEY, 1024).unwrap().is_none());
        assert_eq!(
            StagedAttachments::default()
                .read(&repo, KEY, 1024)
                .unwrap()
                .unwrap(),
            BYTES
        );
    }

    #[test]
    fn malformed_and_over_budget_indexes_are_not_absent_evidence() {
        for oversized in [false, true] {
            let (_dir, repo) = fixture();
            let path = repo.path().join("index");
            let retained = repo.path().join("retained-index");
            std::fs::rename(&path, &retained).unwrap();
            let mut bytes = b"DIRC\0\0\0\x02\0\0\0\0".to_vec();
            if oversized {
                bytes[8..12].copy_from_slice(&(MAX_INDEX_ENTRIES + 1).to_be_bytes());
            }
            std::fs::write(&path, &bytes).unwrap();
            assert!(StagedAttachments::default().read(&repo, KEY, 1024).is_err());
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert!(retained.exists());
        }
    }

    #[test]
    fn unresolved_stages_and_non_regular_entries_are_refused() {
        for stage in 0_u16..=3 {
            let (_dir, repo) = fixture();
            let mut index = repo.index().unwrap();
            let mut entry = index.get_path(Path::new(KEY), 0).unwrap();
            index.remove_path(Path::new(KEY)).unwrap();
            if stage == 0 {
                entry.mode = 0o120000;
            } else {
                entry.flags = (entry.flags & !0x3000) | (stage << 12);
            }
            index.add(&entry).unwrap();
            index.write().unwrap();
            let before = std::fs::read(repo.path().join("index")).unwrap();
            assert!(StagedAttachments::default().read(&repo, KEY, 1024).is_err());
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
    }

    #[test]
    fn missing_staged_object_is_an_error_not_an_absent_attachment() {
        let (_dir, repo) = fixture();
        let oid = repo
            .index()
            .unwrap()
            .get_path(Path::new(KEY), 0)
            .unwrap()
            .id
            .to_string();
        let object = repo.path().join("objects").join(&oid[..2]).join(&oid[2..]);
        let retained = repo.path().join("retained-object");
        std::fs::rename(&object, &retained).unwrap();
        assert!(StagedAttachments::default().read(&repo, KEY, 1024).is_err());
        assert!(!object.exists());
        assert!(retained.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_index_authority_is_preserved_and_refused() {
        let (_dir, repo) = fixture();
        let path = repo.path().join("index");
        let retained = repo.path().join("retained-index");
        std::fs::rename(&path, &retained).unwrap();
        let before = std::fs::read(&retained).unwrap();
        std::os::unix::fs::symlink(&retained, &path).unwrap();
        assert!(StagedAttachments::default().read(&repo, KEY, 1024).is_err());
        assert_eq!(std::fs::read_link(path).unwrap(), retained);
        assert_eq!(std::fs::read(retained).unwrap(), before);
    }
}
