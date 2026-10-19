//! Include uncommitted canonical identities in the publication proof.
//!
//! HEAD and the working tree can both lack a message that survives in Git's
//! index after a failed commit. Inspect a private index snapshot, never refresh
//! or write the real index. Unchanged HEAD entries need no second blob read;
//! changed/staged entries must not hide a different path or payload for this ID.

use std::io::Write;
use std::path::Path;

use git2::{ErrorCode, Index, ObjectType, Oid, Repository, Tree};

use super::{CanonicalScanBudget, invalid};
use crate::ProjectArchive;

const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn preflight(
    repo: &Repository,
    archive: &ProjectArchive,
    message_id: i64,
    canonical: &str,
    expected: Oid,
    head: Option<&Tree<'_>>,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<()> {
    budget.checkpoint("staged index open")?;
    let path = repo.path().join("index");
    if crate::path_existing_prefix_has_symlink(&path)? {
        return Err(invalid("staged canonical index has a symlinked authority"));
    }
    let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
        &path,
        MAX_INDEX_BYTES.min(budget.bytes_left),
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(invalid(format!(
                "staged canonical index read or byte budget failed: {error}",
            )));
        }
    };
    budget.bytes_left = budget
        .bytes_left
        .checked_sub(bytes.len() as u64)
        .ok_or_else(|| invalid("staged canonical index byte budget exceeded"))?;
    let header = bytes
        .get(..12)
        .ok_or_else(|| invalid("staged canonical index has an incomplete header"))?;
    if &header[..4] != b"DIRC" {
        return Err(invalid("staged canonical index has an invalid signature"));
    }
    let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if u64::from(count) > budget.entries_left as u64 {
        return Err(invalid("staged canonical index entry budget exceeded"));
    }
    budget.checkpoint("staged index snapshot")?;
    let mut snapshot = tempfile::Builder::new()
        .prefix(".canonical-identity-index-")
        .tempfile()?;
    snapshot.write_all(&bytes)?;
    drop(bytes);
    budget.checkpoint("staged index decode")?;
    // Keep this private file alive until the decoded index is dropped. Split
    // indexes without their authority and corrupt/unsupported formats fail
    // closed rather than becoming proof of an empty staged namespace.
    let index = Index::open_ext(snapshot.path(), repo.object_format())?;
    if index.len() > budget.entries_left {
        return Err(invalid("staged canonical index decoded entry budget exceeded"));
    }
    let root = crate::archive_project_root_checked(archive)?.join("messages");
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, &root)?;
    let prefix = format!("{relative}/");
    for entry in index.iter() {
        budget.visit()?;
        let component = entry.path.strip_suffix(b"/").unwrap_or(&entry.path);
        if canonical
            .as_bytes()
            .strip_prefix(component)
            .is_some_and(|tail| tail.starts_with(b"/"))
        {
            return Err(invalid(
                "staged entry obstructs the canonical namespace; repair refused",
            ));
        }
        let Some(suffix) = entry.path.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        if entry.mode == 0o040000 {
            return Err(invalid(
                "staged canonical namespace contains an unexpanded sparse directory",
            ));
        }
        if !canonical_artifact(suffix)? {
            continue;
        }
        let path = std::str::from_utf8(&entry.path)
            .map_err(|_| invalid("staged canonical source path is not UTF-8"))?;
        if entry.flags & 0x3000 != 0 {
            return Err(invalid(
                "staged canonical source has unresolved merge stages; preserved",
            ));
        }
        if !matches!(entry.mode, 0o100644 | 0o100755) {
            return Err(invalid(
                "staged canonical source is not a regular-file entry; preserved",
            ));
        }
        if path == canonical {
            if entry.id != expected {
                return Err(invalid(
                    "staged canonical payload conflicts with authoritative message; preserved",
                ));
            }
            // The caller owns these exact bytes and may recreate their blob.
            continue;
        }
        if let Some(head) = head {
            match head.get_path(Path::new(path)) {
                Ok(committed)
                    if committed.kind() == Some(ObjectType::Blob)
                        && matches!(committed.filemode(), 0o100644 | 0o100755)
                        && committed.id() == entry.id =>
                {
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.code() == ErrorCode::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        budget.checkpoint("staged canonical blob")?;
        // Share the caller's actual object-byte budget and object-ID validation
        // with the committed scan; filenames alone never establish identity.
        if super::read_id(repo, entry.id, budget)? == message_id {
            return Err(invalid(format!(
                "canonical message id {message_id} already staged at {path}; repair refused",
            )));
        }
    }
    budget.checkpoint("staged canonical result")?;
    Ok(())
}

fn canonical_artifact(suffix: &[u8]) -> crate::Result<bool> {
    let mut parts = suffix.split(|byte| *byte == b'/');
    let Some(year) = parts.next() else {
        return Ok(false);
    };
    if year.len() != 4 || !year.iter().all(u8::is_ascii_digit) {
        return Ok(false);
    }
    let Some(month) = parts.next() else {
        return Err(invalid("staged canonical year shard is not a directory"));
    };
    if month.len() != 2 || !month.iter().all(u8::is_ascii_digit) {
        return Ok(false);
    }
    let Some(name) = parts.next() else {
        return Err(invalid("staged canonical month shard is not a directory"));
    };
    Ok(parts.next().is_none() && name.ends_with(b".md"))
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::{entry, fixture};
    use super::super::super::{ReconcileResult, reconcile_message_bundle};
    use super::*;
    use serde_json::json;

    fn stage(repo: &Repository, archive: &ProjectArchive, path: &Path, bytes: &[u8]) -> Oid {
        crate::ensure_parent_dir(path).unwrap();
        std::fs::write(path, bytes).unwrap();
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(&relative)).unwrap();
        index.write().unwrap();
        index.get_path(Path::new(&relative), 0).unwrap().id
    }

    #[test]
    fn staged_duplicate_blocks_fresh_and_interrupted_publication_without_writes() {
        for interrupted in [false, true] {
            let (_dir, config, archive, message, recipients) = fixture();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let source = archive.root.join("messages/2025/01/prior.md");
            let bytes = b"---json\n{\"id\":42}\n---\n\nretained staged message";
            let oid = stage(&repo, &archive, &source, bytes);
            let evidence = config.storage_root.join("retained-staged-message.md");
            std::fs::rename(&source, &evidence).unwrap();
            let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                .unwrap()
                .0;
            if interrupted {
                let full = crate::render_message_bundle_content(
                    &message,
                    entry(&message, &recipients).body_md,
                )
                .unwrap();
                crate::ensure_parent_dir(&paths.canonical).unwrap();
                std::fs::write(&paths.canonical, &full).unwrap();
            }
            let head = repo.head().ok().and_then(|head| head.target());
            let index_before = std::fs::read(repo.path().join("index")).unwrap();
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(error.to_string().contains("already staged"), "{error}");
            assert_eq!(repo.head().ok().and_then(|head| head.target()), head);
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), index_before);
            assert_eq!(std::fs::read(evidence).unwrap(), bytes);
            assert_eq!(repo.find_blob(oid).unwrap().content(), bytes);
            assert_eq!(paths.canonical.exists(), interrupted);
            assert!(!paths.outbox.exists());
            assert!(paths.inbox.iter().all(|path| !path.exists()));
        }
    }

    #[test]
    fn conflicting_staged_target_cannot_be_overwritten_by_authoritative_payload() {
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let mut conflicting = message.clone();
        conflicting["reply_to"] = json!(19);
        let bytes = crate::render_message_bundle_content(
            &conflicting,
            entry(&message, &recipients).body_md,
        )
        .unwrap();
        let oid = stage(&repo, &archive, &paths.canonical, bytes.as_bytes());
        let evidence = config.storage_root.join("retained-conflicting-parent.md");
        std::fs::rename(&paths.canonical, &evidence).unwrap();
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
        assert!(error.to_string().contains("staged canonical payload conflicts"), "{error}");
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        assert_eq!(repo.find_blob(oid).unwrap().content(), bytes.as_bytes());
        assert_eq!(std::fs::read(evidence).unwrap(), bytes.as_bytes());
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
    }

    #[test]
    fn exact_staged_target_is_repaired_and_committed_once() {
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let bytes = crate::render_message_bundle_content(
            &message,
            entry(&message, &recipients).body_md,
        )
        .unwrap();
        stage(&repo, &archive, &paths.canonical, bytes.as_bytes());
        let evidence = config.storage_root.join("retained-exact-message.md");
        std::fs::rename(&paths.canonical, &evidence).unwrap();
        let result = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        assert_eq!(std::fs::read(&paths.canonical).unwrap(), bytes.as_bytes());
        assert_eq!(std::fs::read(evidence).unwrap(), bytes.as_bytes());
        assert_eq!(
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn staged_duplicate_is_checked_even_for_an_already_committed_complete_bundle() {
        let (_dir, config, archive, message, recipients) = fixture();
        reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let source = archive.root.join("messages/2025/01/prior.md");
        stage(&repo, &archive, &source, b"---json\n{\"id\":42}\n---\n\nprior");
        std::fs::rename(&source, config.storage_root.join("prior-evidence.md")).unwrap();
        let head = repo.head().unwrap().target();
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
        assert!(error.to_string().contains("already staged"), "{error}");
        assert_eq!(repo.head().unwrap().target(), head);
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
    }

    #[test]
    fn unrelated_staged_message_and_foreign_project_do_not_block_repair() {
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        let bytes = b"---json\n{\"id\":99}\n---\n\nunrelated";
        let oid = stage(&repo, &archive, &source, bytes);
        std::fs::rename(&source, config.storage_root.join("unrelated-evidence.md")).unwrap();
        let foreign = archive.repo_root.join("projects/other/messages/2025/01/unknown.md");
        stage(&repo, &archive, &foreign, b"foreign evidence without frontmatter");
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let expected = Oid::hash_object(ObjectType::Blob, b"not-yet-published").unwrap();
        let mut budget = CanonicalScanBudget::default();
        preflight(&repo, &archive, 42, &relative, expected, None, &mut budget).unwrap();
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        assert_eq!(repo.find_blob(oid).unwrap().content(), bytes);
        assert!(
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_ok()
        );
    }

    #[test]
    fn malformed_staged_evidence_and_unmerged_entries_never_prove_absence() {
        for bytes in [b"broken".as_slice(), b"---json\n{\"id\":0}\n---\n\n".as_slice()] {
            let (_dir, config, archive, message, recipients) = fixture();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let source = archive.root.join("messages/2025/01/uncertain.md");
            stage(&repo, &archive, &source, bytes);
            std::fs::rename(&source, config.storage_root.join("uncertain-evidence.md")).unwrap();
            let before = std::fs::read(repo.path().join("index")).unwrap();
            assert!(reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err());
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let source = archive.root.join("messages/2025/01/conflicted.md");
        stage(&repo, &archive, &source, b"---json\n{\"id\":99}\n---\n\n");
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &source).unwrap();
        let mut index = repo.index().unwrap();
        let mut unresolved = index.get_path(Path::new(&relative), 0).unwrap();
        index.remove_path(Path::new(&relative)).unwrap();
        unresolved.flags = (unresolved.flags & !0x3000) | 0x2000;
        index.add(&unresolved).unwrap();
        index.write().unwrap();
        std::fs::rename(&source, config.storage_root.join("merge-evidence.md")).unwrap();
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
        assert!(error.to_string().contains("merge stages"), "{error}");
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
    }

    #[test]
    fn index_bytes_and_entries_share_the_identity_scan_budget() {
        let (_dir, _config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        let bytes = b"---json\n{\"id\":99}\n---\n\nretained";
        stage(&repo, &archive, &source, bytes);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let expected = Oid::hash_object(ObjectType::Blob, b"new").unwrap();
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let count = repo.index().unwrap().len();
        for mut budget in [
            CanonicalScanBudget { entries_left: 0, ..Default::default() },
            CanonicalScanBudget { bytes_left: 1, ..Default::default() },
            CanonicalScanBudget { bytes_left: before.len() as u64, ..Default::default() },
        ] {
            let error = preflight(&repo, &archive, 42, &relative, expected, None, &mut budget)
                .unwrap_err();
            assert!(error.to_string().contains("budget"), "{error}");
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
        let mut budget = CanonicalScanBudget::default();
        let entries = budget.entries_left;
        let available = budget.bytes_left;
        preflight(&repo, &archive, 42, &relative, expected, None, &mut budget).unwrap();
        assert_eq!(budget.entries_left, entries - count);
        assert_eq!(budget.bytes_left, available - before.len() as u64 - bytes.len() as u64);
    }

    #[test]
    fn corrupt_index_is_preserved_instead_of_treated_as_empty() {
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let index_path = repo.path().join("index");
        if index_path.exists() {
            std::fs::rename(&index_path, config.storage_root.join("retained-original-index")).unwrap();
        }
        std::fs::write(&index_path, b"DIRC\0\0\0\x02\0\0\0\0broken checksum").unwrap();
        let before = std::fs::read(&index_path).unwrap();
        assert!(reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err());
        assert_eq!(std::fs::read(index_path).unwrap(), before);
    }

    #[test]
    fn nonregular_entries_and_staged_namespace_obstructions_are_preserved() {
        for obstruction in [false, true] {
            let (_dir, config, archive, message, recipients) = fixture();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let source = archive.root.join("messages/2025/01/unrelated.md");
            stage(&repo, &archive, &source, b"---json\n{\"id\":99}\n---\n\n");
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &source).unwrap();
            let mut index = repo.index().unwrap();
            let mut uncertain = index.get_path(Path::new(&relative), 0).unwrap();
            index.remove_path(Path::new(&relative)).unwrap();
            if obstruction {
                let root = archive.root.join("messages");
                uncertain.path = crate::rel_path_cached(&archive.canonical_repo_root, &root)
                    .unwrap()
                    .into_bytes();
            } else {
                uncertain.mode = 0o120000;
            }
            index.add(&uncertain).unwrap();
            index.write().unwrap();
            std::fs::rename(&source, config.storage_root.join("retained-uncertain.md")).unwrap();
            let before = std::fs::read(repo.path().join("index")).unwrap();
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            let expected = if obstruction { "obstructs" } else { "regular-file" };
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_index_authority_is_not_followed_or_replaced() {
        let (_dir, config, archive, message, recipients) = fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        stage(&repo, &archive, &source, b"---json\n{\"id\":99}\n---\n\n");
        let index = repo.path().join("index");
        let evidence = config.storage_root.join("retained-index-authority");
        std::fs::rename(&index, &evidence).unwrap();
        std::os::unix::fs::symlink(&evidence, &index).unwrap();
        let before = std::fs::read(&evidence).unwrap();
        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
        assert!(error.to_string().contains("symlinked authority"), "{error}");
        assert_eq!(std::fs::read_link(index).unwrap(), evidence);
        assert_eq!(std::fs::read(evidence).unwrap(), before);
    }
}
