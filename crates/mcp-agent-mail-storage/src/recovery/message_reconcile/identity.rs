//! Canonical message identity survives loss of the archive working tree.
//!
//! Checking only the proposed bundle paths misses an older date/subject path
//! carrying the same ID. Inspect one immutable HEAD tree, within this project's
//! canonical namespace, before publishing files or first committing a bundle.

mod staged;

use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository, Tree};

use super::{CanonicalScanBudget, MAX_MESSAGE_ARTIFACT_BYTES, invalid};
use crate::ProjectArchive;

/// An unborn branch is not proof that the archive has no recorded history.
/// A damaged or redirected HEAD may coexist with branch, tag, or packed refs
/// carrying identities that disappeared from the working tree. Do not choose
/// an arbitrary replacement ref: the operator must restore HEAD's authority.
/// Only a genuinely unborn HEAD with no enumerated refs permits first publish.
/// Missing/malformed HEAD and ref-iterator errors remain unavailable evidence.
fn head_tree_for_identity<'repo>(
    repo: &'repo Repository,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<Option<Tree<'repo>>> {
    budget.checkpoint("git root")?;
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree()?)),
        Err(error) if error.code() == ErrorCode::UnbornBranch => {
            budget.checkpoint("git unborn refs")?;
            if repo.references()?.next().transpose()?.is_some() {
                return Err(invalid(
                    "archive HEAD is unborn but retained Git refs remain; repair refused",
                ));
            }
            budget.checkpoint("git unborn result")?;
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// An immutable canonical-namespace observation, not publication authority.
/// The exact namespace must still be current when the project lock is held;
/// staged entries and working-tree evidence are checked independently then.
pub(super) struct CommittedIdentityProof {
    namespace: String,
    namespace_tree: Option<Oid>,
    message_id: i64,
    canonical: String,
    expected: Oid,
    canonical_committed: bool,
    history_scanned: bool,
}

/// `Tree::get_path` reports NotFound both for a missing name and when an
/// ancestor exists but is not a directory. Only the former proves absence.
/// Walk verified tree entries explicitly, without following symlinks or
/// gitlinks, and propagate object-read failures instead of discarding them.
/// Keep at most one additional native tree; a returned entry owns its data.
fn committed_path_entry(
    repo: &Repository,
    tree: Option<&Tree<'_>>,
    relative: &str,
    budget: &CanonicalScanBudget<'_>,
) -> crate::Result<Option<git2::TreeEntry<'static>>> {
    budget.checkpoint("git path")?;
    let mut components = relative.split('/').peekable();
    if Path::new(relative).is_absolute()
        || relative.contains(['\\', '\0'])
        || components.clone().any(|name| matches!(name, "" | "." | ".."))
    {
        return Err(invalid("committed canonical path is not a relative Git path"));
    }
    let Some(root) = tree else {
        return Ok(None);
    };
    let mut parent: Option<Tree<'_>> = None;
    while let Some(name) = components.next() {
        budget.checkpoint("git path component")?;
        let next = {
            let current = parent.as_ref().unwrap_or(root);
            let Some(entry) = current.get_name(name) else {
                return Ok(None);
            };
            if components.peek().is_none() {
                return Ok(Some(entry.to_owned()));
            }
            if entry.kind() != Some(ObjectType::Tree) || entry.filemode() != 0o040000 {
                return Err(invalid(format!(
                    "committed canonical ancestor {name} is not a directory; preserved"
                )));
            }
            entry.id()
        };
        // An entry pointing at an unavailable object is not a missing name.
        // Do not downgrade this failure to the successful absence result.
        drop(parent.take());
        parent = Some(repo.find_tree(next)?);
    }
    Err(invalid("committed canonical path has no terminal entry"))
}

fn canonical_is_committed(
    repo: &Repository,
    tree: Option<&Tree<'_>>,
    canonical: &str,
    expected: Oid,
    budget: &CanonicalScanBudget<'_>,
) -> crate::Result<bool> {
    match committed_path_entry(repo, tree, canonical, budget)? {
        Some(entry)
            if entry.kind() == Some(ObjectType::Blob)
                && matches!(entry.filemode(), 0o100644 | 0o100755)
                && entry.id() == expected =>
        {
            Ok(true)
        }
        Some(_) => Err(invalid(format!(
            "committed archive artifact {canonical} conflicts with authoritative message; preserved"
        ))),
        None => Ok(false),
    }
}

fn namespace_tree(
    repo: &Repository,
    tree: Option<&Tree<'_>>,
    relative: &str,
    budget: &CanonicalScanBudget<'_>,
) -> crate::Result<Option<Oid>> {
    let Some(entry) = committed_path_entry(repo, tree, relative, budget)? else {
        return Ok(None);
    };
    if entry.kind() != Some(ObjectType::Tree) || entry.filemode() != 0o040000 {
        return Err(invalid(
            "committed canonical namespace is not a directory; repair refused",
        ));
    }
    Ok(Some(entry.id()))
}

/// Check historical immutable blobs without retaining the mutation fence.
/// Missing/malformed objects and exhausted budgets never produce a proof.
/// Only the exact content-addressed project namespace may be reused later;
/// neither HEAD movement elsewhere nor an epoch token proves this namespace.
pub(super) fn prepare_committed(
    repo: &Repository,
    archive: &ProjectArchive,
    message_id: i64,
    canonical: &str,
    expected: Oid,
    message_files_missing: bool,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<CommittedIdentityProof> {
    let tree = head_tree_for_identity(repo, budget)?;
    let root = crate::archive_project_root_checked(archive)?.join("messages");
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, &root)?;
    if !canonical.starts_with(&format!("{relative}/")) {
        return Err(invalid(
            "committed canonical target is outside its project namespace",
        ));
    }
    let canonical_committed =
        canonical_is_committed(repo, tree.as_ref(), canonical, expected, budget)?;
    let namespace_tree = namespace_tree(repo, tree.as_ref(), &relative, budget)?;
    let proof = CommittedIdentityProof {
        namespace: relative.clone(),
        namespace_tree,
        message_id,
        canonical: canonical.to_string(),
        expected,
        canonical_committed,
        history_scanned: !canonical_committed || message_files_missing,
    };
    // A fully materialized, already-recorded message (including an attachment-
    // only repair) introduces no canonical path. Do not rescan history on every
    // unchanged maintenance visit. Missing message copies still require proof.
    if canonical_committed && !message_files_missing {
        budget.checkpoint("git scan result")?;
        return Ok(proof);
    }
    let Some(namespace_tree) = namespace_tree else {
        budget.checkpoint("git scan result")?;
        return Ok(proof);
    };
    let root = repo.find_tree(namespace_tree)?;
    for (year, year_id) in date_trees(&root, 4, budget)? {
        budget.checkpoint("git year")?;
        let year_tree = repo.find_tree(year_id)?;
        for (month, month_id) in date_trees(&year_tree, 2, budget)? {
            budget.checkpoint("git month")?;
            let month_tree = repo.find_tree(month_id)?;
            for entry in &month_tree {
                budget.visit()?;
                let name = entry.name_bytes();
                if !name.ends_with(b".md") {
                    continue;
                }
                if entry.kind() != Some(ObjectType::Blob)
                    || !matches!(entry.filemode(), 0o100644 | 0o100755)
                {
                    return Err(invalid(
                        "committed canonical source is not a regular-file blob; repair refused",
                    ));
                }
                let name = std::str::from_utf8(name)
                    .map_err(|_| invalid("committed canonical source path is not UTF-8"))?;
                let path = format!("{relative}/{year}/{month}/{name}");
                // Exact target bytes were already checked against the caller's
                // authoritative payload. A missing target object can be repaired
                // from that payload; a missing OTHER object is uncertain identity.
                if path == canonical {
                    continue;
                }
                if read_id(repo, entry.id(), budget)? == message_id {
                    return Err(invalid(format!(
                        "canonical message id {message_id} already committed at {path}; repair refused",
                    )));
                }
            }
        }
    }
    budget.checkpoint("git scan result")?;
    Ok(proof)
}

/// Consume the immutable proof under the caller's publication/project locks.
/// A changed namespace or newly missing copy requires a fresh bounded pass;
/// never fall back to a historical scan while the global fence is held.
pub(super) fn revalidate_prepared(
    repo: &Repository,
    archive: &ProjectArchive,
    proof: &CommittedIdentityProof,
    message_files_missing: bool,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<bool> {
    budget.checkpoint("git proof revalidation")?;
    let tree = head_tree_for_identity(repo, budget)?;
    let root = crate::archive_project_root_checked(archive)?.join("messages");
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, &root)?;
    if relative != proof.namespace
        || namespace_tree(repo, tree.as_ref(), &relative, budget)? != proof.namespace_tree
        || (message_files_missing && !proof.history_scanned)
    {
        return Err(crate::StorageError::LockContention {
            message: "canonical archive identity changed after preparation; retry required".into(),
        });
    }
    // Even a complete committed bundle can coexist with conflicting staged
    // evidence. Read current index bytes only after taking publication locks.
    staged::preflight(
        repo,
        archive,
        proof.message_id,
        &proof.canonical,
        proof.expected,
        tree.as_ref(),
        budget,
    )?;
    budget.checkpoint("git scan result")?;
    Ok(proof.canonical_committed)
}

/// Return whether this exact canonical payload is already recorded in HEAD.
/// Read-only observers and standalone checks retain the same shared bounds.
pub(super) fn preflight(
    repo: &Repository,
    archive: &ProjectArchive,
    message_id: i64,
    canonical: &str,
    expected: Oid,
    message_files_missing: bool,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<bool> {
    let proof = prepare_committed(
        repo,
        archive,
        message_id,
        canonical,
        expected,
        message_files_missing,
        budget,
    )?;
    revalidate_prepared(repo, archive, &proof, message_files_missing, budget)
}

fn date_trees(
    tree: &Tree<'_>,
    width: usize,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<Vec<(String, Oid)>> {
    budget.checkpoint("git directory")?;
    let mut entries = Vec::new();
    for entry in tree {
        budget.visit()?;
        let name = entry.name_bytes();
        if name.len() != width || !name.iter().all(u8::is_ascii_digit) {
            continue;
        }
        if entry.kind() != Some(ObjectType::Tree) || entry.filemode() != 0o040000 {
            return Err(invalid(
                "committed canonical date shard is not a directory; repair refused",
            ));
        }
        // ASCII digits were established above; retain a fallible conversion
        // rather than relying on an unchecked path or a lossy identity.
        let name = std::str::from_utf8(name)
            .map_err(|_| invalid("committed canonical date shard is not UTF-8"))?;
        entries.push((name.to_string(), entry.id()));
    }
    budget.checkpoint("git directory result")?;
    Ok(entries)
}

fn read_id(
    repo: &Repository,
    oid: Oid,
    budget: &mut CanonicalScanBudget<'_>,
) -> crate::Result<i64> {
    // libgit2 materializes blobs. Bound and charge the entire object before
    // loading it, even though only its frontmatter is decoded. Never trust a
    // filename's embedded ID or treat an unreadable object as an absent message.
    budget.checkpoint("git header")?;
    let (size, kind) = repo.odb()?.read_header(oid)?;
    if kind != ObjectType::Blob
        || size > MAX_MESSAGE_ARTIFACT_BYTES
        || size as u64 > budget.bytes_left
    {
        return Err(invalid(
            "committed canonical scan byte budget exceeded or source is not a blob; repair refused",
        ));
    }
    budget.checkpoint("git blob")?;
    budget.bytes_left -= size as u64;
    let blob = repo.find_blob(oid)?;
    budget.checkpoint("git digest")?;
    if blob.content().len() != size
        || Oid::hash_object_ext(ObjectType::Blob, blob.content(), oid.object_format())? != oid
    {
        return Err(invalid(
            "committed canonical content does not match its object identity; repair refused",
        ));
    }
    budget.checkpoint("git decode")?;
    let text = std::str::from_utf8(blob.content())
        .map_err(|_| invalid("committed canonical source is not UTF-8"))?;
    let (frontmatter, _) = text
        .strip_prefix("---json\n")
        .and_then(|text| text.split_once("\n---\n"))
        .ok_or_else(|| invalid("committed canonical source has invalid frontmatter"))?;
    let message: serde_json::Value = serde_json::from_str(frontmatter)?;
    budget.checkpoint("git result")?;
    crate::positive_message_id(&message)
        .ok_or_else(|| invalid("committed canonical source has no positive message ID"))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{entry, fixture};
    use super::super::{ReconcileResult, reconcile_message_bundle};
    use super::*;
    use crate::Config;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    fn commit_source(archive: &ProjectArchive, config: &Config, path: &Path, bytes: &[u8]) -> Oid {
        crate::ensure_parent_dir(path).unwrap();
        std::fs::write(path, bytes).unwrap();
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
        crate::commit_paths_with_retry(
            &archive.repo_root,
            config,
            "fixture: retained canonical identity",
            &[relative.as_str()],
        )
        .unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap();
        head.target().unwrap()
    }

    fn materialize_bundle(
        archive: &ProjectArchive,
        message: &Value,
        recipients: &[String],
    ) -> Vec<(PathBuf, Vec<u8>)> {
        let paths = crate::message_paths_for_bundle(archive, message, "BlueLake", recipients)
            .unwrap()
            .0;
        let full =
            crate::render_message_bundle_content(message, entry(message, recipients).body_md)
                .unwrap()
                .into_bytes();
        let inbox = crate::render_message_bundle_content(
            &crate::redact_message_bcc_for_inbox(message),
            entry(message, recipients).body_md,
        )
        .unwrap()
        .into_bytes();
        let mut files = vec![(paths.canonical, full.clone()), (paths.outbox, full)];
        files.extend(paths.inbox.into_iter().map(|path| (path, inbox.clone())));
        for (path, bytes) in &files {
            crate::ensure_parent_dir(path).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        files
    }

    fn tree_with_entry(repo: &Repository, path: &str, mode: i32) -> (Oid, Oid) {
        let target = if mode == 0o160000 {
            let empty = repo.treebuilder(None).unwrap().write().unwrap();
            let tree = repo.find_tree(empty).unwrap();
            let signature = git2::Signature::now("recovery", "recovery@example.invalid").unwrap();
            repo.commit(
                None,
                &signature,
                &signature,
                "gitlink witness",
                &tree,
                &[],
            )
            .unwrap()
        } else {
            repo.blob(b"retained committed ancestor evidence").unwrap()
        };
        let mut oid = target;
        let mut entry_mode = mode;
        for name in path.rsplit('/') {
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert(name, oid, entry_mode).unwrap();
            oid = builder.write().unwrap();
            entry_mode = 0o040000;
        }
        (oid, target)
    }

    fn commit_ancestor(repo: &Repository, path: &str, mode: i32) -> (Oid, Oid) {
        let (root, target) = tree_with_entry(repo, path, mode);
        let tree = repo.find_tree(root).unwrap();
        let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
        let parents: Vec<_> = parent.iter().collect();
        let signature = git2::Signature::now("recovery", "recovery@example.invalid").unwrap();
        let head = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "ancestor witness",
                &tree,
                &parents,
            )
            .unwrap();
        (head, target)
    }

    #[test]
    fn committed_path_lookup_distinguishes_absence_from_nondirectory_ancestors() {
        for mode in [0o100644, 0o100755, 0o120000, 0o160000] {
            let dir = tempfile::TempDir::new().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let (root, target) = tree_with_entry(&repo, "projects/project", mode);
            let tree = repo.find_tree(root).unwrap();
            let budget = CanonicalScanBudget::default();
            let path = "projects/project/messages/2026/01/message.md";
            // This native error is exactly why a blanket NotFound => absent
            // conversion is insufficient, even with a fully readable ODB.
            assert_eq!(
                tree.get_path(Path::new(path)).err().map(|error| error.code()),
                Some(ErrorCode::NotFound)
            );
            let error = committed_path_entry(&repo, Some(&tree), path, &budget)
                .err()
                .expect("an existing non-directory ancestor is conflicting evidence");
            assert!(
                error
                    .to_string()
                    .contains("ancestor project is not a directory")
            );
            assert!(canonical_is_committed(&repo, Some(&tree), path, target, &budget).is_err());
            assert!(
                namespace_tree(&repo, Some(&tree), "projects/project/messages", &budget).is_err()
            );
            for absent in ["missing/child", "projects/missing/child"] {
                assert!(
                    committed_path_entry(&repo, Some(&tree), absent, &budget)
                        .unwrap()
                        .is_none()
                );
            }
            let retained = committed_path_entry(&repo, Some(&tree), "projects/project", &budget)
                .unwrap()
                .unwrap();
            assert_eq!((retained.id(), retained.filemode()), (target, mode));
        }
    }

    #[test]
    fn committed_path_lookup_rejects_invalid_paths_even_without_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let budget = CanonicalScanBudget::default();
        for path in [
            "",
            ".",
            "..",
            "/projects",
            "missing//child",
            "missing/../child",
            "missing/./child",
            "missing/",
            "missing\\child",
            "missing/\0child",
        ] {
            assert!(
                committed_path_entry(&repo, None, path, &budget).is_err(),
                "{path:?}"
            );
        }
        assert!(
            committed_path_entry(&repo, None, "projects/project/messages", &budget)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unavailable_intermediate_tree_is_not_absence_and_restoration_reenables_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let (root, target) = tree_with_entry(&repo, "projects/project", 0o100644);
        let tree = repo.find_tree(root).unwrap();
        let subtree = tree.get_name("projects").unwrap().id().to_string();
        let object = repo
            .path()
            .join("objects")
            .join(&subtree[..2])
            .join(&subtree[2..]);
        let retained = dir.path().join("retained-subtree-object");
        let before = std::fs::read(&object).unwrap();
        drop(tree);
        drop(repo);
        // Preserve the actual loose object, rather than substituting a mock
        // lookup failure. Reopen to exclude the previous repository's cache.
        std::fs::rename(&object, &retained).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        let tree = repo.find_tree(root).unwrap();
        let budget = CanonicalScanBudget::default();
        assert!(committed_path_entry(&repo, Some(&tree), "projects/project", &budget).is_err());
        assert!(!object.exists());
        assert_eq!(std::fs::read(&retained).unwrap(), before);
        drop(tree);
        drop(repo);
        std::fs::rename(&retained, &object).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        let tree = repo.find_tree(root).unwrap();
        assert_eq!(
            committed_path_entry(&repo, Some(&tree), "projects/project", &budget)
                .unwrap()
                .unwrap()
                .id(),
            target
        );
    }

    #[test]
    fn missing_expected_leaf_blob_still_allows_authoritative_repair() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = "messages/message.md";
        let (root, target) = tree_with_entry(&repo, path, 0o100644);
        let hex = target.to_string();
        let object = repo.path().join("objects").join(&hex[..2]).join(&hex[2..]);
        let retained = dir.path().join("retained-target-blob");
        let before = std::fs::read(&object).unwrap();
        drop(repo);
        std::fs::rename(&object, &retained).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        let tree = repo.find_tree(root).unwrap();
        assert!(repo.find_blob(target).is_err());
        // The existing contract permits recreating this exact leaf from the
        // authoritative bundle. Do not require that leaf object to be present
        // merely to distinguish missing parents from absent path components.
        assert!(
            canonical_is_committed(
                &repo,
                Some(&tree),
                path,
                target,
                &CanonicalScanBudget::default(),
            )
            .unwrap()
        );
        assert!(!object.exists());
        assert_eq!(std::fs::read(retained).unwrap(), before);
    }

    #[test]
    fn committed_ancestor_conflicts_refuse_repairs_without_changing_any_bundle_copy() {
        for mode in [0o100644, 0o100755, 0o120000, 0o160000] {
            for interrupted in [false, true] {
                let (_dir, config, archive, message, recipients) = fixture();
                crate::flush_async_commits();
                let repo = Repository::open(&archive.repo_root).unwrap();
                let ancestor =
                    crate::rel_path_cached(&archive.canonical_repo_root, &archive.root).unwrap();
                let (before, target) = commit_ancestor(&repo, &ancestor, mode);
                let index_before = std::fs::read(repo.path().join("index")).ok();
                let files = if interrupted {
                    materialize_bundle(&archive, &message, &recipients)
                } else {
                    Vec::new()
                };
                let paths =
                    crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                        .unwrap()
                        .0;
                let error =
                    reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                        .unwrap_err();
                assert!(
                    error.to_string().contains("committed canonical ancestor"),
                    "{error}"
                );
                assert_eq!(repo.head().unwrap().target(), Some(before));
                assert_eq!(std::fs::read(repo.path().join("index")).ok(), index_before);
                let head = repo.head().unwrap().peel_to_tree().unwrap();
                let witness = head.get_path(Path::new(&ancestor)).unwrap();
                assert_eq!((witness.id(), witness.filemode()), (target, mode));
                if interrupted {
                    for (path, bytes) in files {
                        assert_eq!(std::fs::read(path).unwrap(), bytes);
                    }
                } else {
                    assert!(!paths.canonical.exists());
                    assert!(!paths.outbox.exists());
                    assert!(paths.inbox.iter().all(|path| !path.exists()));
                }
            }
        }
    }

    #[test]
    fn a_missing_namespace_proof_cannot_survive_a_new_committed_ancestor_conflict() {
        let (_dir, _config, archive, message, recipients) = fixture();
        crate::flush_async_commits();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let canonical =
            crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let full =
            crate::render_message_bundle_content(&message, entry(&message, &recipients).body_md)
                .unwrap();
        let expected = Oid::hash_object(ObjectType::Blob, full.as_bytes()).unwrap();
        let mut budget = CanonicalScanBudget::default();
        let proof = prepare_committed(&repo, &archive, 42, &canonical, expected, true, &mut budget)
            .unwrap();
        assert!(
            proof.namespace_tree.is_none(),
            "fixture begins without committed messages"
        );
        let ancestor =
            crate::rel_path_cached(&archive.canonical_repo_root, &archive.root).unwrap();
        let (before, _) = commit_ancestor(&repo, &ancestor, 0o100644);
        let error = revalidate_prepared(&repo, &archive, &proof, true, &mut budget).unwrap_err();
        assert!(
            error.to_string().contains("committed canonical ancestor"),
            "{error}"
        );
        assert_eq!(repo.head().unwrap().target(), Some(before));
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
    }

    #[test]
    fn fresh_unborn_head_without_refs_still_permits_first_publication() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut options = git2::RepositoryInitOptions::new();
        options.initial_head("new-archive");
        let repo = Repository::init_opts(dir.path(), &options).unwrap();
        // `git2::Reference` is not `Debug`, so `unwrap_err` cannot compile.
        assert_eq!(
            repo.head().err().as_ref().map(git2::Error::code),
            Some(ErrorCode::UnbornBranch)
        );
        let mut budget = CanonicalScanBudget::default();
        assert!(
            head_tree_for_identity(&repo, &mut budget)
                .unwrap()
                .is_none()
        );
        assert!(repo.references().unwrap().next().is_none());
    }

    #[test]
    fn missing_head_is_not_an_empty_identity_namespace() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let head = repo.path().join("HEAD");
        let evidence = dir.path().join("preserved-HEAD");
        let before = std::fs::read(&head).unwrap();
        // Retain the opened repository to exercise the exact head-read error,
        // not merely Repository::open's earlier refusal of an invalid layout.
        std::fs::rename(&head, &evidence).unwrap();
        let mut budget = CanonicalScanBudget::default();
        assert!(head_tree_for_identity(&repo, &mut budget).is_err());
        assert!(!head.exists());
        assert_eq!(std::fs::read(evidence).unwrap(), before);
    }

    #[test]
    fn unborn_head_with_loose_or_packed_tag_authority_is_not_empty() {
        for packed in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let oid = repo.blob(b"retained object authority").unwrap();
            if packed {
                std::fs::write(
                    repo.path().join("packed-refs"),
                    format!("{oid} refs/tags/retained-identity\n"),
                )
                .unwrap();
            } else {
                repo.reference("refs/tags/retained-identity", oid, false, "fixture")
                    .unwrap();
            }
            assert_eq!(
                repo.head().err().as_ref().map(git2::Error::code),
                Some(ErrorCode::UnbornBranch)
            );
            let mut budget = CanonicalScanBudget::default();
            let error = head_tree_for_identity(&repo, &mut budget).unwrap_err();
            assert!(error.to_string().contains("retained Git refs"), "{error}");
            assert_eq!(
                repo.find_reference("refs/tags/retained-identity")
                    .unwrap()
                    .target(),
                Some(oid)
            );
            assert_eq!(
                repo.find_blob(oid).unwrap().content(),
                b"retained object authority"
            );
        }
    }

    #[test]
    fn unborn_head_cannot_hide_duplicate_identity_or_publish_partial_repairs() {
        for interrupted in [false, true] {
            let (_dir, config, archive, message, recipients) = fixture();
            let source = archive.root.join("messages/2025/01/prior.md");
            let prior = b"---json\n{\"id\": 42}\n---\n\nprior generation";
            let before = commit_source(&archive, &config, &source, prior);
            let evidence = config.storage_root.join("retained-prior-generation.md");
            std::fs::rename(&source, &evidence).unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let original_ref = repo.head().unwrap().name().unwrap().to_string();
            repo.set_head("refs/heads/unborn-recovery").unwrap();
            let head_before = std::fs::read(repo.path().join("HEAD")).unwrap();
            let files = if interrupted {
                materialize_bundle(&archive, &message, &recipients)
            } else {
                Vec::new()
            };
            let paths =
                crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                    .unwrap()
                    .0;
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(error.to_string().contains("retained Git refs"), "{error}");
            assert_eq!(
                repo.find_reference(&original_ref).unwrap().target(),
                Some(before)
            );
            assert_eq!(
                std::fs::read(repo.path().join("HEAD")).unwrap(),
                head_before
            );
            assert_eq!(std::fs::read(&evidence).unwrap(), prior);
            if interrupted {
                for (path, bytes) in files {
                    assert_eq!(std::fs::read(path).unwrap(), bytes);
                }
            } else {
                assert!(!paths.canonical.exists());
                assert!(!paths.outbox.exists());
                assert!(paths.inbox.iter().all(|path| !path.exists()));
            }
            // Restoring the known ref makes the original duplicate visible;
            // the failed repair must not have established a new canonical ID.
            repo.set_head(&original_ref).unwrap();
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(
                error.to_string().contains("canonical message id 42"),
                "{error}"
            );
            assert_eq!(repo.head().unwrap().target(), Some(before));
        }
    }

    #[test]
    fn restoring_head_allows_nonconflicting_reconciliation_again() {
        let (_dir, config, archive, message, recipients) = fixture();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        let bytes = b"---json\n{\"id\": 99}\n---\n\nretained message";
        commit_source(&archive, &config, &source, bytes);
        std::fs::rename(&source, config.storage_root.join("retained-unrelated.md")).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let original_ref = repo.head().unwrap().name().unwrap().to_string();
        repo.set_head("refs/heads/unborn-recovery").unwrap();
        assert!(reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err());
        repo.set_head(&original_ref).unwrap();
        let result =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        assert_eq!(
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn committed_duplicate_survives_worktree_loss_and_interrupted_retry() {
        for interrupted in [false, true] {
            let (_dir, config, archive, message, recipients) = fixture();
            let mut prior = message.clone();
            prior["created"] = json!("2025-01-02T03:04:05Z");
            prior["subject"] = json!("previous generation");
            let old = crate::message_paths_for_bundle(&archive, &prior, "BlueLake", &recipients)
                .unwrap()
                .0;
            let bytes = crate::render_message_bundle_content(&prior, "prior generation").unwrap();
            let before = commit_source(&archive, &config, &old.canonical, bytes.as_bytes());
            let evidence = config.storage_root.join("retained-prior-generation.md");
            std::fs::rename(&old.canonical, &evidence).unwrap();
            let files = if interrupted {
                materialize_bundle(&archive, &message, &recipients)
            } else {
                Vec::new()
            };
            let paths =
                crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                    .unwrap()
                    .0;
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("canonical message id 42 already committed"),
                "interrupted={interrupted}: {error}"
            );
            if interrupted {
                for (path, expected) in files {
                    assert_eq!(std::fs::read(path).unwrap(), expected);
                }
            } else {
                assert!(!paths.canonical.exists());
                assert!(!paths.outbox.exists());
                assert!(paths.inbox.iter().all(|path| !path.exists()));
            }
            assert_eq!(std::fs::read(&evidence).unwrap(), bytes.as_bytes());
            let repo = Repository::open(&archive.repo_root).unwrap();
            assert_eq!(repo.head().unwrap().target().unwrap(), before);
        }
    }

    #[test]
    fn interrupted_retry_checks_uncommitted_duplicate_even_when_no_file_is_missing() {
        let (_dir, config, archive, message, recipients) = fixture();
        let source = archive.root.join("messages/2025/01/prior.md");
        let prior = b"---json\n{\"id\": 42}\n---\n\nprior evidence";
        crate::ensure_parent_dir(&source).unwrap();
        std::fs::write(&source, prior).unwrap();
        let files = materialize_bundle(&archive, &message, &recipients);
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().ok().and_then(|head| head.target());
        let error =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap_err();
        assert!(error.to_string().contains("canonical message id 42"));
        assert_eq!(repo.head().ok().and_then(|head| head.target()), before);
        assert_eq!(std::fs::read(source).unwrap(), prior);
        for (path, bytes) in files {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn committed_scan_is_project_scoped_and_accepts_other_message_ids() {
        let (_dir, config, archive, message, recipients) = fixture();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        let bytes = b"---json\n{\"id\": 99}\n---\n\nunrelated evidence";
        commit_source(&archive, &config, &source, bytes);
        std::fs::rename(&source, config.storage_root.join("retained-unrelated.md")).unwrap();
        let foreign = archive
            .repo_root
            .join("projects/other/messages/2025/01/uncertain.md");
        commit_source(&archive, &config, &foreign, b"not canonical frontmatter");
        let result =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"not canonical frontmatter"
        );
        assert_eq!(
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn malformed_committed_canonical_evidence_cannot_be_hidden_by_disk_loss() {
        let malformed: &[&[u8]] = &[
            b"not canonical frontmatter",
            b"---json\n{broken json}\n---\n\n",
            b"---json\n{\"id\": 0}\n---\n\n",
            b"---json\n{\"subject\": \"identity missing\"}\n---\n\n",
            b"---json\n{\"id\": 99}\n",
        ];
        for bytes in malformed {
            let (_dir, config, archive, message, recipients) = fixture();
            let source = archive.root.join("messages/2025/01/uncertain.md");
            let before = commit_source(&archive, &config, &source, bytes);
            let evidence = config.storage_root.join("retained-malformed.md");
            std::fs::rename(&source, &evidence).unwrap();
            assert!(
                reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err()
            );
            let paths =
                crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                    .unwrap()
                    .0;
            assert!(!paths.canonical.exists());
            assert!(!paths.outbox.exists());
            assert!(paths.inbox.iter().all(|path| !path.exists()));
            assert_eq!(std::fs::read(evidence).unwrap(), *bytes);
            let repo = Repository::open(&archive.repo_root).unwrap();
            assert_eq!(repo.head().unwrap().target().unwrap(), before);
        }
    }

    #[test]
    fn committed_scan_exhaustion_is_not_an_empty_namespace() {
        let (_dir, config, archive, message, recipients) = fixture();
        let source = archive.root.join("messages/2025/01/unrelated.md");
        let bytes = b"---json\n{\"id\": 99}\n---\n\nretained";
        let before = commit_source(&archive, &config, &source, bytes);
        let repo = Repository::open(&archive.repo_root).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let relative =
            crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let full =
            crate::render_message_bundle_content(&message, entry(&message, &recipients).body_md)
                .unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, full.as_bytes()).unwrap();
        for mut budget in [
            CanonicalScanBudget {
                entries_left: 2,
                bytes_left: 2048,
                ..Default::default()
            },
            CanonicalScanBudget {
                entries_left: 10,
                bytes_left: 1,
                ..Default::default()
            },
        ] {
            let error =
                preflight(&repo, &archive, 42, &relative, oid, true, &mut budget).unwrap_err();
            assert!(error.to_string().contains("budget"), "{error}");
        }
        // Retain the same HEAD allowance after charging the real index snapshot.
        // Only the one canonical source is enumerated in the index, not
        // bootstrap files elsewhere in the repository-wide index.
        let index_path = repo.path().join("index");
        let (index_bytes, index_entries) = if index_path.exists() {
            (std::fs::metadata(&index_path).unwrap().len(), 1)
        } else {
            (0, 0)
        };
        let mut budget = CanonicalScanBudget {
            entries_left: 10 + index_entries,
            bytes_left: 2048 + index_bytes,
            ..Default::default()
        };
        assert!(!preflight(&repo, &archive, 42, &relative, oid, true, &mut budget).unwrap());
        assert_eq!(budget.entries_left, 7);
        assert_eq!(budget.bytes_left, 2048 - bytes.len() as u64);
        assert_eq!(std::fs::read(source).unwrap(), bytes);
        assert!(!paths.canonical.exists());
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }

    #[test]
    fn prepared_history_keeps_its_consumed_budget_for_guarded_index_checks() {
        let (_dir, config, archive, message, recipients) = fixture();
        let source = archive.root.join("messages/2025/01/other.md");
        let bytes = b"---json\n{\"id\":99}\n---\n\nretained";
        let before = commit_source(&archive, &config, &source, bytes);
        let repo = Repository::open(&archive.repo_root).unwrap();
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &source).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(&relative)).unwrap();
        index.write().unwrap();
        drop(index);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let canonical =
            crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let full =
            crate::render_message_bundle_content(&message, entry(&message, &recipients).body_md)
                .unwrap();
        let expected = Oid::hash_object(ObjectType::Blob, full.as_bytes()).unwrap();
        let mut budget = CanonicalScanBudget {
            entries_left: 3,
            ..Default::default()
        };
        let proof = prepare_committed(&repo, &archive, 42, &canonical, expected, true, &mut budget)
            .unwrap();
        assert_eq!(budget.entries_left, 0, "year, month, and canonical blob");
        assert_eq!(
            budget.bytes_left,
            super::super::MAX_CANONICAL_SCAN_BYTES - bytes.len() as u64
        );
        let error = revalidate_prepared(&repo, &archive, &proof, true, &mut budget).unwrap_err();
        assert!(error.to_string().contains("entry budget"), "{error}");
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn newly_missing_message_copy_requires_a_full_fresh_history_proof() {
        let (_dir, config, archive, message, recipients) = fixture();
        reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let canonical =
            crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        let full = std::fs::read(&paths.canonical).unwrap();
        let expected = Oid::hash_object(ObjectType::Blob, &full).unwrap();
        let mut budget = CanonicalScanBudget::default();
        let proof = prepare_committed(
            &repo,
            &archive,
            42,
            &canonical,
            expected,
            false,
            &mut budget,
        )
        .unwrap();
        std::fs::rename(
            &paths.outbox,
            config.storage_root.join("retained-outbox.md"),
        )
        .unwrap();
        let missing = super::super::artifact_missing(&paths.outbox, &full).unwrap();
        let error = revalidate_prepared(&repo, &archive, &proof, missing, &mut budget).unwrap_err();
        assert!(matches!(error, crate::StorageError::LockContention { .. }));
        assert!(
            !paths.outbox.exists(),
            "an incomplete proof authorizes no repair"
        );
        let repaired =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(repaired.files_created, 1);
        assert_eq!(std::fs::read(&paths.outbox).unwrap(), full);
    }
}
