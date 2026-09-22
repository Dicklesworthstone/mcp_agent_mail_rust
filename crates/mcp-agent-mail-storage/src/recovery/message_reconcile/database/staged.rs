//! Recover archive-only metadata staged before a failed Git commit.
//!
//! The index is a last-resort surviving copy, never a replacement for live DB
//! identity/routing or committed evidence. Read one bounded private snapshot;
//! never refresh, repair, or write the real index while selecting metadata.

use std::io::Write;
use std::path::{Path, PathBuf};

use git2::{Index, ObjectType, Oid, Repository};
use serde_json::Value;

use super::{
    PreparedMessage, merge_surviving_metadata, restore_inbox_metadata, validate_surviving_message,
};
use crate::ProjectArchive;

const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_ENTRIES: u32 = 100_000;

struct StagedMessages {
    index: Index,
    // Keep the private snapshot alive until libgit2 has released the index.
    _snapshot: tempfile::NamedTempFile,
}

impl StagedMessages {
    fn open(repo: &Repository) -> Result<Option<Self>, String> {
        let path = repo.path().join("index");
        if crate::path_existing_prefix_has_symlink(&path).map_err(|error| error.to_string())? {
            return Err("staged archive index has a symlinked authority".to_string());
        }
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &path,
            MAX_INDEX_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("staged archive index is unavailable: {error}")),
        };
        let header = bytes
            .get(..12)
            .ok_or_else(|| "staged archive index has an incomplete header".to_string())?;
        if &header[..4] != b"DIRC" {
            return Err("staged archive index has an invalid signature".to_string());
        }
        let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        if count > MAX_INDEX_ENTRIES {
            return Err("staged archive index entry budget exceeded".to_string());
        }
        let mut snapshot = tempfile::Builder::new()
            .prefix(".message-reconcile-index-")
            .tempfile()
            .map_err(|error| error.to_string())?;
        snapshot
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        drop(bytes);
        // Unsupported/corrupt formats (including an unavailable split index)
        // are errors, not an empty index or permission to invent reply fields.
        let index = Index::open_ext(snapshot.path(), repo.object_format())
            .map_err(|error| format!("staged archive index cannot be decoded: {error}"))?;
        if index.len() > MAX_INDEX_ENTRIES as usize {
            return Err("staged archive index decoded entry budget exceeded".to_string());
        }
        Ok(Some(Self {
            index,
            _snapshot: snapshot,
        }))
    }

    fn read(
        &self,
        repo: &Repository,
        archive: &ProjectArchive,
        path: &Path,
        remaining: &mut usize,
    ) -> Result<Option<(Value, String)>, String> {
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path)
            .map_err(|error| error.to_string())?;
        let key = Path::new(&relative);
        if (1..=3).any(|stage| self.index.get_path(key, stage).is_some()) {
            return Err(
                "staged archive message has unresolved merge stages; preserved".to_string(),
            );
        }
        let Some(entry) = self.index.get_path(key, 0) else {
            return Ok(None);
        };
        if entry.path != relative.as_bytes() || !matches!(entry.mode, 0o100644 | 0o100755) {
            return Err("staged archive message is not an exact regular-file entry".to_string());
        }
        let odb = repo.odb().map_err(|error| error.to_string())?;
        let (size, kind) = odb
            .read_header(entry.id)
            .map_err(|error| error.to_string())?;
        if kind != ObjectType::Blob
            || size > super::super::MAX_MESSAGE_ARTIFACT_BYTES
            || size > *remaining
        {
            return Err(
                "staged archive message byte budget exceeded or object is not a blob".to_string(),
            );
        }
        *remaining -= size;
        let blob = repo
            .find_blob(entry.id)
            .map_err(|error| error.to_string())?;
        if blob.content().len() != size
            || Oid::hash_object_ext(ObjectType::Blob, blob.content(), entry.id.object_format())
                .map_err(|error| error.to_string())?
                != entry.id
        {
            return Err("staged archive content does not match its object identity".to_string());
        }
        let text = std::str::from_utf8(blob.content())
            .map_err(|_| "staged archive message is not UTF-8".to_string())?;
        let (frontmatter, body) = text
            .strip_prefix("---json\n")
            .and_then(|text| text.split_once("\n---\n\n"))
            .ok_or_else(|| {
                "staged archive message has invalid canonical frontmatter".to_string()
            })?;
        let message = serde_json::from_str(frontmatter)
            .map_err(|_| "staged archive message has invalid JSON".to_string())?;
        Ok(Some((message, body.to_string())))
    }

    fn reject_other_canonical_id(
        &self,
        repo: &Repository,
        archive: &ProjectArchive,
        canonical: &Path,
        message_id: i64,
        remaining: &mut usize,
    ) -> Result<(), String> {
        let root = crate::archive_project_root_checked(archive)
            .map_err(|error| error.to_string())?
            .join("messages");
        let prefix = format!(
            "{}/",
            crate::rel_path_cached(&archive.canonical_repo_root, &root)
                .map_err(|error| error.to_string())?,
        );
        // The snapshot's decoded entry count is already bounded. Canonical
        // objects share the metadata byte budget rather than getting a new one.
        for entry in self.index.iter() {
            let Some(suffix) = entry.path.strip_prefix(prefix.as_bytes()) else {
                continue;
            };
            let suffix = std::str::from_utf8(suffix)
                .map_err(|_| "staged canonical path is not UTF-8".to_string())?;
            let mut parts = suffix.split('/');
            let (Some(year), Some(month), Some(name), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if year.len() != 4
                || month.len() != 2
                || !year
                    .bytes()
                    .chain(month.bytes())
                    .all(|byte| byte.is_ascii_digit())
                || !name.ends_with(".md")
            {
                continue;
            }
            let path = root.join(suffix);
            if path == canonical {
                continue;
            }
            let (message, _) = self
                .read(repo, archive, &path, remaining)?
                .ok_or_else(|| "staged canonical observation lost its entry".to_string())?;
            let id = crate::positive_message_id(&message)
                .ok_or_else(|| "staged canonical source has no positive message ID".to_string())?;
            if id == message_id {
                return Err(format!(
                    "canonical message id {id} already staged at another path; repair refused",
                ));
            }
        }
        Ok(())
    }
}

/// Called only after all working-tree and committed metadata sources are absent.
///
/// Validate every staged candidate, including redacted inboxes, before returning
/// a consensus. A different parent/extension is conflicting evidence, not a vote.
pub(super) fn read_metadata(
    repo: &Repository,
    archive: &ProjectArchive,
    prepared: &PreparedMessage,
    canonical: &Path,
    outbox: &Path,
    inboxes: &[PathBuf],
) -> Result<Option<Value>, String> {
    let Some(snapshot) = StagedMessages::open(repo)? else {
        return Ok(None);
    };
    let mut remaining = super::super::MAX_BUNDLE_BYTES;
    let mut surviving = None;
    for (path, inbox) in [(canonical, false), (outbox, false)]
        .into_iter()
        .chain(inboxes.iter().map(|path| (path.as_path(), true)))
    {
        if let Some((message, body)) = snapshot.read(repo, archive, path, &mut remaining)? {
            let message = if inbox {
                restore_inbox_metadata(prepared, message, &body)?
            } else {
                validate_surviving_message(prepared, &message, &body)?;
                message
            };
            merge_surviving_metadata(
                &mut surviving,
                message,
                "staged archive metadata disagree; all evidence preserved",
            )?;
        }
    }
    if surviving.is_some() {
        let id = crate::positive_message_id(&prepared.message)
            .ok_or_else(|| "staged recovery requires a positive source ID".to_string())?;
        snapshot.reject_other_canonical_id(repo, archive, canonical, id, &mut remaining)?;
    }
    Ok(surviving)
}

#[cfg(test)]
mod tests {
    use super::super::tests::git_fixture;
    use super::super::{ReconcileResult, read_surviving_message, reconcile_prepared};
    use super::*;
    use serde_json::json;

    fn stage_and_retain(
        repo: &Repository,
        archive: &ProjectArchive,
        path: &Path,
        message: &Value,
        body: &str,
        evidence: &Path,
    ) -> Vec<u8> {
        let bytes = crate::render_message_bundle_content(message, body)
            .unwrap()
            .into_bytes();
        crate::ensure_parent_dir(path).unwrap();
        std::fs::write(path, &bytes).unwrap();
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(&relative)).unwrap();
        index.write().unwrap();
        std::fs::rename(path, evidence).unwrap();
        bytes
    }

    #[test]
    fn staged_only_reply_restores_exact_metadata_and_bcc_without_resending() {
        for source in ["canonical", "outbox", "inbox"] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let mut full = original.message.clone();
            full["reply_to"] = json!(7);
            full["future_metadata"] = json!({"opaque": ["keep", 42]});
            let redacted = crate::redact_message_bcc_for_inbox(&full);
            let (path, message) = match source {
                "canonical" => (&paths.canonical, &full),
                "outbox" => (&paths.outbox, &full),
                _ => (&paths.inbox[0], &redacted),
            };
            let repo = Repository::open(&archive.repo_root).unwrap();
            let evidence = config.storage_root.join("retained-staged-reply.md");
            let bytes = stage_and_retain(&repo, &archive, path, message, &original.body, &evidence);
            assert!(
                super::super::CommittedMessages::open(&archive)
                    .unwrap()
                    .read(&archive, path)
                    .unwrap()
                    .is_none()
            );
            let result = reconcile_prepared(&config, &original).unwrap();
            assert_eq!(result.files_created, 4, "{source}");
            assert!(result.git_commit_needed, "{source}");
            for path in [&paths.canonical, &paths.outbox] {
                let (message, body) = read_surviving_message(path).unwrap().unwrap();
                assert_eq!(message, full);
                assert_eq!(body, original.body);
            }
            for path in &paths.inbox {
                let (message, body) = read_surviving_message(path).unwrap().unwrap();
                assert_eq!(message, redacted);
                assert_eq!(message["bcc"], json!([]));
                assert_eq!(body, original.body);
            }
            assert_eq!(std::fs::read(&evidence).unwrap(), bytes);
            let head = repo.head().unwrap().target().unwrap();
            assert_eq!(
                reconcile_prepared(&config, &original).unwrap(),
                ReconcileResult::default()
            );
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        }
    }

    #[test]
    fn staged_replies_survive_commits_and_process_restarts() {
        const CHILD_ROOT: &str = "AM_TEST_STAGED_REPAIR_ROOT";
        const CHILD_INPUT: &str = "AM_TEST_STAGED_REPAIR_INPUT";
        const COMPLETED: &str = "staged reply repair completed";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let input: Value = serde_json::from_str(&std::env::var(CHILD_INPUT).unwrap()).unwrap();
            let prepared = PreparedMessage {
                message: input["message"].clone(),
                body: input["body"].as_str().unwrap().to_string(),
                sender: input["sender"].as_str().unwrap().to_string(),
                project_slug: input["project_slug"].as_str().unwrap().to_string(),
                recipients: serde_json::from_value(input["recipients"].clone()).unwrap(),
                archive_metadata_known: false,
                payload_bytes: 512,
            };
            let config = mcp_agent_mail_core::Config {
                storage_root: root.into(),
                ..mcp_agent_mail_core::Config::default()
            };
            let result = reconcile_prepared(&config, &prepared).unwrap();
            assert_eq!(
                result.files_created,
                usize::try_from(input["expected_created"].as_u64().unwrap()).unwrap()
            );
            assert_eq!(result.git_commit_needed, result.files_created != 0);
            println!("{COMPLETED}");
            return;
        }

        for ordinary_commit_first in [false, true] {
            let (_temp, config, original, archive) = git_fixture();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let mut messages = Vec::new();
            // More than one reconciliation batch; no snapshot or Rust state
            // from one repair process can rescue the next process's metadata.
            for id in 9..15 {
                let mut message = original.message.clone();
                message["id"] = json!(id);
                let mut full = message.clone();
                full["reply_to"] = json!(7);
                full["future_metadata"] = json!({"opaque": ["retain", id]});
                let paths = crate::message_paths_for_bundle(
                    &archive,
                    &message,
                    &original.sender,
                    &original.recipients,
                )
                .unwrap()
                .0;
                let evidence = config.storage_root.join(format!("retained-reply-{id}.md"));
                let bytes = stage_and_retain(
                    &repo,
                    &archive,
                    &paths.canonical,
                    &full,
                    &original.body,
                    &evidence,
                );
                let input = json!({
                    "message": message,
                    "body": original.body,
                    "sender": original.sender,
                    "project_slug": original.project_slug,
                    "recipients": original.recipients,
                    "expected_created": 4,
                });
                messages.push((input, full, paths, evidence, bytes));
            }
            drop(repo);
            if ordinary_commit_first {
                let unrelated = archive.root.join("recovery-witness.txt");
                std::fs::write(&unrelated, "ordinary archive activity").unwrap();
                let relative =
                    crate::rel_path_cached(&archive.canonical_repo_root, &unrelated).unwrap();
                crate::commit_paths_with_retry(
                    &archive.repo_root,
                    &config,
                    "unrelated archive activity",
                    &[relative.as_str()],
                )
                .unwrap();
            }
            for (input, full, paths, evidence, bytes) in &messages {
                let run_child = |input: &Value| {
                    let output = std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "recovery::message_reconcile::database::staged::tests::staged_replies_survive_commits_and_process_restarts",
                            "--nocapture",
                        ])
                        .env(CHILD_ROOT, &config.storage_root)
                        .env(CHILD_INPUT, serde_json::to_string(input).unwrap())
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "ordinary_commit_first={ordinary_commit_first}, id={}: {}{}",
                        input["message"]["id"],
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETED));
                };
                run_child(input);
                for path in [&paths.canonical, &paths.outbox] {
                    let (restored, body) = read_surviving_message(path).unwrap().unwrap();
                    assert_eq!(&restored, full);
                    assert_eq!(body, original.body);
                }
                for path in &paths.inbox {
                    let (restored, body) = read_surviving_message(path).unwrap().unwrap();
                    assert_eq!(restored, crate::redact_message_bcc_for_inbox(full));
                    assert_eq!(restored["bcc"], json!([]));
                    assert_eq!(body, original.body);
                }
                assert_eq!(&std::fs::read(evidence).unwrap(), bytes);
                let repo = Repository::open(&archive.repo_root).unwrap();
                let head = repo.head().unwrap().target().unwrap();
                let tree = repo.find_commit(head).unwrap().tree().unwrap();
                for path in [&paths.canonical, &paths.outbox]
                    .into_iter()
                    .chain(paths.inbox.iter())
                {
                    let relative =
                        crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
                    let entry = tree.get_path(Path::new(&relative)).unwrap();
                    assert_eq!(
                        repo.find_blob(entry.id()).unwrap().content(),
                        std::fs::read(path).unwrap(),
                    );
                }
                let mut repeated = input.clone();
                repeated["expected_created"] = json!(0);
                run_child(&repeated);
                assert_eq!(repo.head().unwrap().target().unwrap(), head);
            }
        }
    }

    #[test]
    fn staged_disagreement_preserves_head_index_and_all_retained_evidence() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let repo = Repository::open(&archive.repo_root).unwrap();
        let mut full = original.message.clone();
        full["reply_to"] = json!(7);
        let first = config.storage_root.join("retained-full.md");
        let first_bytes = stage_and_retain(
            &repo,
            &archive,
            &paths.canonical,
            &full,
            &original.body,
            &first,
        );
        let mut inbox = crate::redact_message_bcc_for_inbox(&full);
        inbox["reply_to"] = json!(8);
        let second = config.storage_root.join("retained-inbox.md");
        let second_bytes = stage_and_retain(
            &repo,
            &archive,
            &paths.inbox[0],
            &inbox,
            &original.body,
            &second,
        );
        let index_before = std::fs::read(repo.path().join("index")).unwrap();
        let head_before = repo.head().ok().and_then(|head| head.target());
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("staged archive metadata disagree"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(repo.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(repo.head().ok().and_then(|head| head.target()), head_before);
        assert_eq!(std::fs::read(first).unwrap(), first_bytes);
        assert_eq!(std::fs::read(second).unwrap(), second_bytes);
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
    }

    #[test]
    fn staged_only_duplicate_canonical_id_refuses_before_publication() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let repo = Repository::open(&archive.repo_root).unwrap();
        let mut full = original.message.clone();
        full["reply_to"] = json!(7);
        let inbox = crate::redact_message_bcc_for_inbox(&full);
        stage_and_retain(
            &repo,
            &archive,
            &paths.inbox[0],
            &inbox,
            &original.body,
            &config.storage_root.join("retained-inbox.md"),
        );
        let other = archive.root.join("messages/2025/01/other__9.md");
        stage_and_retain(
            &repo,
            &archive,
            &other,
            &full,
            &original.body,
            &config.storage_root.join("retained-other.md"),
        );
        let index_before = std::fs::read(repo.path().join("index")).unwrap();
        let head_before = repo.head().ok().and_then(|head| head.target());
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("canonical message id 9 already staged"),
            "{error}"
        );
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
        assert_eq!(
            std::fs::read(repo.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(repo.head().ok().and_then(|head| head.target()), head_before);
    }

    #[test]
    fn staged_inbox_cannot_authorize_changed_body_identity_routing_or_bcc() {
        for field in ["body", "id", "project_slug", "to", "bcc"] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let repo = Repository::open(&archive.repo_root).unwrap();
            let mut message = crate::redact_message_bcc_for_inbox(&original.message);
            message["reply_to"] = json!(7);
            match field {
                "id" => message["id"] = json!(10),
                "project_slug" => message["project_slug"] = json!("foreign"),
                "to" => message["to"] = json!(["RedFox"]),
                "bcc" => message["bcc"] = json!(["RedFox"]),
                _ => {}
            }
            let body = if field == "body" {
                "different"
            } else {
                &original.body
            };
            let evidence = config.storage_root.join("retained-invalid.md");
            let bytes =
                stage_and_retain(&repo, &archive, &paths.inbox[0], &message, body, &evidence);
            let index_before = std::fs::read(repo.path().join("index")).unwrap();
            let head_before = repo.head().ok().and_then(|head| head.target());
            assert!(reconcile_prepared(&config, &original).is_err(), "{field}");
            assert!(!paths.canonical.exists());
            assert!(!paths.outbox.exists());
            assert!(paths.inbox.iter().all(|path| !path.exists()));
            assert_eq!(std::fs::read(evidence).unwrap(), bytes);
            assert_eq!(
                std::fs::read(repo.path().join("index")).unwrap(),
                index_before
            );
            assert_eq!(repo.head().ok().and_then(|head| head.target()), head_before);
        }
    }

    #[test]
    fn staged_snapshot_does_not_mix_index_generations() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let repo = Repository::open(&archive.repo_root).unwrap();
        let mut first = original.message.clone();
        first["reply_to"] = json!(7);
        stage_and_retain(
            &repo,
            &archive,
            &paths.canonical,
            &first,
            &original.body,
            &config.storage_root.join("retained-first.md"),
        );
        let snapshot = StagedMessages::open(&repo).unwrap().unwrap();
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        stage_and_retain(
            &repo,
            &archive,
            &paths.canonical,
            &second,
            &original.body,
            &config.storage_root.join("retained-second.md"),
        );
        let mut remaining = super::super::super::MAX_BUNDLE_BYTES;
        let (observed, _) = snapshot
            .read(&repo, &archive, &paths.canonical, &mut remaining)
            .unwrap()
            .unwrap();
        assert_eq!(observed, first);
        let fresh = StagedMessages::open(&repo).unwrap().unwrap();
        let (observed, _) = fresh
            .read(&repo, &archive, &paths.canonical, &mut remaining)
            .unwrap()
            .unwrap();
        assert_eq!(observed, second);
    }

    #[test]
    fn staged_conflicts_nonregular_entries_and_missing_objects_are_not_absence() {
        for problem in ["merge", "symlink", "missing"] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let repo = Repository::open(&archive.repo_root).unwrap();
            let mut message = original.message.clone();
            message["reply_to"] = json!(7);
            stage_and_retain(
                &repo,
                &archive,
                &paths.canonical,
                &message,
                &original.body,
                &config.storage_root.join("retained-staged.md"),
            );
            let relative =
                crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
            // A repository-owned index validates object existence during add.
            // Open the fixture index without an ODB so the missing-object case
            // reaches the reconciler instead of failing during fixture setup.
            let mut index =
                Index::open_ext(&repo.path().join("index"), repo.object_format()).unwrap();
            let mut entry = index.get_path(Path::new(&relative), 0).unwrap();
            match problem {
                "merge" => entry.flags = (entry.flags & !0x3000) | 0x1000,
                "symlink" => entry.mode = 0o120000,
                _ => {
                    entry.id = Oid::hash_object(ObjectType::Blob, b"not stored in this repository")
                        .unwrap();
                    assert!(!repo.odb().unwrap().exists(entry.id));
                }
            }
            index.add(&entry).unwrap();
            index.write().unwrap();
            if problem == "merge" {
                assert!(index.get_path(Path::new(&relative), 1).is_some());
            }
            let before = std::fs::read(repo.path().join("index")).unwrap();
            assert!(reconcile_prepared(&config, &original).is_err(), "{problem}");
            assert!(!paths.canonical.exists());
            assert!(!paths.outbox.exists());
            assert!(paths.inbox.iter().all(|path| !path.exists()));
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
    }

    #[test]
    fn staged_index_and_blob_budgets_refuse_without_mutation() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let repo = Repository::open(&archive.repo_root).unwrap();
        stage_and_retain(
            &repo,
            &archive,
            &paths.canonical,
            &original.message,
            &original.body,
            &config.storage_root.join("retained-staged.md"),
        );
        let snapshot = StagedMessages::open(&repo).unwrap().unwrap();
        assert!(
            snapshot
                .read(&repo, &archive, &paths.canonical, &mut 1)
                .unwrap_err()
                .contains("budget")
        );
        let index_path = repo.path().join("index");
        let saved = config.storage_root.join("retained-index");
        std::fs::rename(&index_path, &saved).unwrap();
        let mut oversized_header = b"DIRC\0\0\0\x02".to_vec();
        oversized_header.extend_from_slice(&(MAX_INDEX_ENTRIES + 1).to_be_bytes());
        std::fs::write(&index_path, &oversized_header).unwrap();
        assert!(StagedMessages::open(&repo).is_err());
        assert_eq!(std::fs::read(&index_path).unwrap(), oversized_header);
        let file = std::fs::File::create(&index_path).unwrap();
        file.set_len(MAX_INDEX_BYTES + 1).unwrap();
        assert!(StagedMessages::open(&repo).is_err());
        assert_eq!(file.metadata().unwrap().len(), MAX_INDEX_BYTES + 1);
        assert!(!paths.canonical.exists());
        assert!(saved.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staged_index_symlink_is_refused_without_following_or_changing_it() {
        let (_temp, config, _original, archive) = git_fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let path = repo.path().join("index");
        if path.exists() {
            std::fs::rename(&path, config.storage_root.join("retained-index")).unwrap();
        }
        let outside = config.storage_root.join("outside-index");
        std::fs::write(&outside, b"untouched sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(StagedMessages::open(&repo).is_err());
        assert_eq!(std::fs::read_link(&path).unwrap(), outside);
        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched sentinel");
    }
}
