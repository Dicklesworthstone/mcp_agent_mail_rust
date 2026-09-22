//! Canonical message identity survives loss of the archive working tree.
//!
//! Checking only the proposed bundle paths misses an older date/subject path
//! carrying the same ID. Inspect one immutable HEAD tree, within this project's
//! canonical namespace, before publishing files or first committing a bundle.

use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository, Tree};

use super::{CanonicalScanBudget, MAX_MESSAGE_ARTIFACT_BYTES, invalid};
use crate::ProjectArchive;

/// Return whether this exact canonical payload is already recorded in HEAD.
/// Missing or malformed objects and exhausted budgets never prove absence.
/// The caller retains the project lock and shares the remaining budget with
/// its working-tree scan before publication. No Git objects or refs are written.
pub(super) fn preflight(
    repo: &Repository,
    archive: &ProjectArchive,
    message_id: i64,
    canonical: &str,
    expected: Oid,
    message_files_missing: bool,
    budget: &mut CanonicalScanBudget,
) -> crate::Result<bool> {
    let head = match repo.head() {
        Ok(head) => head,
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let tree = head.peel_to_tree()?;
    let canonical_committed = match tree.get_path(Path::new(canonical)) {
        Ok(entry)
            if entry.kind() == Some(ObjectType::Blob)
                && matches!(entry.filemode(), 0o100644 | 0o100755)
                && entry.id() == expected =>
        {
            true
        }
        Ok(_) => {
            return Err(invalid(
                "committed canonical payload conflicts with authoritative message; preserved",
            ));
        }
        Err(error) if error.code() == ErrorCode::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    // A fully materialized, already-recorded message (including an attachment-
    // only repair) introduces no canonical path. Do not rescan history on every
    // unchanged maintenance visit. Missing message copies still require proof.
    if canonical_committed && !message_files_missing {
        return Ok(true);
    }
    let root = crate::archive_project_root_checked(archive)?.join("messages");
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, &root)?;
    let entry = match tree.get_path(Path::new(&relative)) {
        Ok(entry) => entry,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(canonical_committed),
        Err(error) => return Err(error.into()),
    };
    if entry.kind() != Some(ObjectType::Tree) || entry.filemode() != 0o040000 {
        return Err(invalid(
            "committed canonical namespace is not a directory; repair refused",
        ));
    }
    let root = repo.find_tree(entry.id())?;
    for (year, year_id) in date_trees(&root, 4, budget)? {
        let year_tree = repo.find_tree(year_id)?;
        for (month, month_id) in date_trees(&year_tree, 2, budget)? {
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
    Ok(canonical_committed)
}

fn date_trees(
    tree: &Tree<'_>,
    width: usize,
    budget: &mut CanonicalScanBudget,
) -> crate::Result<Vec<(String, Oid)>> {
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
    Ok(entries)
}

fn read_id(repo: &Repository, oid: Oid, budget: &mut CanonicalScanBudget) -> crate::Result<i64> {
    // libgit2 materializes blobs. Bound and charge the entire object before
    // loading it, even though only its frontmatter is decoded. Never trust a
    // filename's embedded ID or treat an unreadable object as an absent message.
    let (size, kind) = repo.odb()?.read_header(oid)?;
    if kind != ObjectType::Blob
        || size > MAX_MESSAGE_ARTIFACT_BYTES
        || size as u64 > budget.bytes_left
    {
        return Err(invalid(
            "committed canonical scan byte budget exceeded or source is not a blob; repair refused",
        ));
    }
    budget.bytes_left -= size as u64;
    let blob = repo.find_blob(oid)?;
    if blob.content().len() != size
        || Oid::hash_object_ext(ObjectType::Blob, blob.content(), oid.object_format())? != oid
    {
        return Err(invalid(
            "committed canonical content does not match its object identity; repair refused",
        ));
    }
    let text = std::str::from_utf8(blob.content())
        .map_err(|_| invalid("committed canonical source is not UTF-8"))?;
    let (frontmatter, _) = text
        .strip_prefix("---json\n")
        .and_then(|text| text.split_once("\n---\n"))
        .ok_or_else(|| invalid("committed canonical source has invalid frontmatter"))?;
    let message: serde_json::Value = serde_json::from_str(frontmatter)?;
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

    fn commit_source(
        archive: &ProjectArchive,
        config: &Config,
        path: &Path,
        bytes: &[u8],
    ) -> Oid {
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
        let full = crate::render_message_bundle_content(message, entry(message, recipients).body_md)
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
            let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                .unwrap()
                .0;
            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(
                error.to_string().contains("canonical message id 42 already committed"),
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
        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
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
        let result = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        assert_eq!(std::fs::read(&foreign).unwrap(), b"not canonical frontmatter");
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
            let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
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
            },
            CanonicalScanBudget {
                entries_left: 10,
                bytes_left: 1,
            },
        ] {
            let error = preflight(&repo, &archive, 42, &relative, oid, true, &mut budget)
                .unwrap_err();
            assert!(error.to_string().contains("budget"), "{error}");
        }
        let mut budget = CanonicalScanBudget {
            entries_left: 10,
            bytes_left: 2048,
        };
        assert!(!preflight(&repo, &archive, 42, &relative, oid, true, &mut budget).unwrap());
        assert_eq!(budget.entries_left, 7);
        assert_eq!(budget.bytes_left, 2048 - bytes.len() as u64);
        assert_eq!(std::fs::read(source).unwrap(), bytes);
        assert!(!paths.canonical.exists());
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }
}
