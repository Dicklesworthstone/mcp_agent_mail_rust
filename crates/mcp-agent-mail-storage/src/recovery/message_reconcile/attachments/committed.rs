//! Read a committed attachment, including a provably identical surviving file.
//!
//! An absent blob does not erase the identity recorded in the pinned HEAD tree.
//! A local copy may supply its bytes only if hashing the complete file recreates
//! that exact object ID. Corruption, unreadable objects and exhausted budgets are
//! not absence. This module never writes an object, index, ref, or working file;
//! the caller must still check metadata and preflight the entire message bundle.

use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository};

use super::super::invalid;

/// The caller has already validated this entry as a regular-file blob and
/// confined `path` to the project's attachments. A surviving file is only a
/// source of bytes, never a replacement for the committed object's identity.
pub(super) fn read(
    repo: &Repository,
    oid: Oid,
    path: &Path,
    remaining_bytes: usize,
) -> crate::Result<Vec<u8>> {
    if let Some(bytes) = read_object(repo, oid, remaining_bytes)? {
        return Ok(bytes);
    }
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid(
            "committed attachment recovery refuses a symlinked surviving file",
        ));
    }
    let bytes = mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
        path,
        remaining_bytes as u64,
    )?;
    if Oid::hash_object_ext(ObjectType::Blob, &bytes, oid.object_format())? != oid {
        return Err(invalid(
            "surviving attachment does not match its committed Git object identity",
        ));
    }
    Ok(bytes)
}

/// Only an explicit NotFound permits a surviving-file read. In particular, a
/// corrupt loose object is not replaced merely because valid local bytes exist.
fn read_object(
    repo: &Repository,
    oid: Oid,
    remaining_bytes: usize,
) -> crate::Result<Option<Vec<u8>>> {
    let (size, kind) = match repo.odb()?.read_header(oid) {
        Ok(header) => header,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if kind != ObjectType::Blob || size > remaining_bytes {
        return Err(invalid(
            "committed attachment object is not a blob or exceeds its byte budget",
        ));
    }
    let blob = match repo.find_blob(oid) {
        Ok(blob) => blob,
        // Loss between the header probe and the read still needs the same
        // exact-ID proof; other read failures retain their original error.
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if blob.content().len() != size
        || Oid::hash_object_ext(ObjectType::Blob, blob.content(), oid.object_format())? != oid
    {
        return Err(invalid(
            "attachment content does not match its Git object identity",
        ));
    }
    Ok(Some(blob.content().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn loose_path(repo: &Repository, oid: Oid) -> PathBuf {
        let hex = oid.to_string();
        repo.path().join("objects").join(&hex[..2]).join(&hex[2..])
    }

    fn assert_absent(repo: &Repository, oid: Oid) {
        assert_eq!(
            repo.odb().unwrap().read_header(oid).unwrap_err().code(),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn missing_object_reads_exact_binary_survivor_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let bytes = b"retained binary\x00\xff\n";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let path = dir.path().join("attachment.bin");
        std::fs::write(&path, bytes).unwrap();
        let head = std::fs::read(repo.path().join("HEAD")).unwrap();
        assert_absent(&repo, oid);

        assert_eq!(read(&repo, oid, &path, bytes.len()).unwrap(), bytes);
        assert_absent(&repo, oid);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(repo.path().join("HEAD")).unwrap(), head);
        assert!(!repo.path().join("index").exists());
    }

    #[test]
    fn readable_object_wins_without_consulting_the_surviving_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let oid = repo.blob(b"before").unwrap();
        let path = dir.path().join("attachment.bin");
        assert_eq!(read(&repo, oid, &path, 6).unwrap(), b"before");
        std::fs::write(&path, b"after!").unwrap();
        assert_eq!(read(&repo, oid, &path, 6).unwrap(), b"before");
        assert_eq!(std::fs::read(&path).unwrap(), b"after!");
    }

    #[test]
    fn missing_object_rejects_same_length_different_bytes_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, b"before").unwrap();
        let path = dir.path().join("attachment.bin");
        assert!(read(&repo, oid, &path, 6).is_err());
        std::fs::write(&path, b"after!").unwrap();
        let error = read(&repo, oid, &path, 6).unwrap_err();
        assert!(error.to_string().contains("committed Git object identity"));
        assert_absent(&repo, oid);
        assert_eq!(std::fs::read(path).unwrap(), b"after!");
    }

    #[test]
    fn both_sources_enforce_the_byte_budget_and_allow_an_empty_blob() {
        for present in [false, true] {
            for bytes in [b"12345".as_slice(), b"".as_slice()] {
                let dir = tempfile::tempdir().unwrap();
                let repo = Repository::init(dir.path()).unwrap();
                let oid = if present {
                    repo.blob(bytes).unwrap()
                } else {
                    Oid::hash_object(ObjectType::Blob, bytes).unwrap()
                };
                let path = dir.path().join("attachment.bin");
                std::fs::write(&path, bytes).unwrap();
                if !bytes.is_empty() {
                    assert!(read(&repo, oid, &path, bytes.len() - 1).is_err());
                }
                assert_eq!(read(&repo, oid, &path, bytes.len()).unwrap(), bytes);
                if !present {
                    assert_absent(&repo, oid);
                }
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn a_non_blob_object_cannot_fall_back_to_a_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let oid = repo.odb().unwrap().write(ObjectType::Tree, &[]).unwrap();
        let path = dir.path().join("attachment.bin");
        std::fs::write(&path, b"").unwrap();
        let error = read(&repo, oid, &path, 0).unwrap_err();
        assert!(error.to_string().contains("not a blob"));
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }

    #[test]
    fn corrupt_or_misaddressed_objects_cannot_be_replaced_by_a_valid_survivor() {
        for misaddressed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let oid = repo.blob(b"before").unwrap();
            let object = loose_path(&repo, oid);
            let retained = dir.path().join("retained-object");
            let original = std::fs::read(&object).unwrap();
            let replacement = if misaddressed {
                let other = repo.blob(b"after!").unwrap();
                std::fs::read(loose_path(&repo, other)).unwrap()
            } else {
                b"not a compressed Git object".to_vec()
            };
            std::fs::rename(&object, &retained).unwrap();
            std::fs::write(&object, &replacement).unwrap();
            let path = dir.path().join("attachment.bin");
            std::fs::write(&path, b"before").unwrap();
            // A fresh handle must observe the damaged on-disk object, not a
            // blob cached while constructing the fixture.
            drop(repo);
            let repo = Repository::open(dir.path()).unwrap();
            assert!(read(&repo, oid, &path, 6).is_err());
            assert_eq!(std::fs::read(&object).unwrap(), replacement);
            assert_eq!(std::fs::read(retained).unwrap(), original);
            assert_eq!(std::fs::read(path).unwrap(), b"before");
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_object_refuses_symlinked_files_and_parent_directories() {
        for parent_link in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let target_dir = dir.path().join("retained");
            std::fs::create_dir(&target_dir).unwrap();
            let target = target_dir.join("attachment.bin");
            std::fs::write(&target, b"before").unwrap();
            let link = dir.path().join("link");
            let (link_target, path) = if parent_link {
                (target_dir, link.join("attachment.bin"))
            } else {
                (target.clone(), link.clone())
            };
            std::os::unix::fs::symlink(&link_target, &link).unwrap();
            let oid = Oid::hash_object(ObjectType::Blob, b"before").unwrap();
            assert!(read(&repo, oid, &path, 6).is_err());
            assert_eq!(std::fs::read_link(link).unwrap(), link_target);
            assert_eq!(std::fs::read(target).unwrap(), b"before");
            assert_absent(&repo, oid);
        }
    }
}
