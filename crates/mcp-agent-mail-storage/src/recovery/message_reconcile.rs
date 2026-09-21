//! Repair missing message artifacts from an authoritative message bundle.
//!
//! Unlike replaying `send_message`, reconciliation never delivers mail, emits
//! notifications, updates read receipts, or appends another thread digest entry.
//! Existing conflicting bytes are evidence, not an invitation to overwrite.
//! A successful result includes an independently checked Git HEAD, not merely
//! an enqueue into the best-effort commit queue.

pub mod database;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository};
use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};

use crate::{MessageBundleBatchEntry, ProjectArchive, StorageError};

/// Maximum serialized bytes for one canonical message artifact.
pub const MAX_MESSAGE_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
/// Bound fan-out as well as the size of an individual message.
const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECIPIENTS: usize = 1024;

/// A completed reconciliation, not a queued write or a new delivery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileResult {
    pub files_created: usize,
    pub git_commit_needed: bool,
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidPath(message.into())
}

/// Restore missing canonical/outbox/inbox copies without replacing any file.
///
/// The caller must supply authoritative frontmatter, including optional fields
/// such as `reply_to` when known. This function deliberately does not infer
/// those fields from a thread ID. Existing serializers, path validation,
/// project locks, BCC redaction and canonical-ID collision checks are reused.
/// Attachment bytes and thread digests are not reconstructed here.
///
/// # Errors
///
/// Refuses malformed identity/time/recipient metadata, excessive fan-out,
/// symlinks, conflicting artifacts, canonical-ID collisions, publication errors,
/// and unverified Git commits. Already-published files survive a later error;
/// retrying checks them again rather than duplicating or rolling them back.
pub fn reconcile_message_bundle(
    archive: &ProjectArchive,
    config: &Config,
    entry: MessageBundleBatchEntry<'_>,
) -> crate::Result<ReconcileResult> {
    let id = crate::positive_message_id(entry.message)
        .ok_or_else(|| invalid("archive reconciliation requires a positive message ID"))?;
    let created = entry
        .message
        .get("created")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("archive reconciliation requires the original created timestamp"))?;
    chrono::DateTime::parse_from_rfc3339(created)
        .map_err(|_| invalid("archive reconciliation refuses an invalid created timestamp"))?;
    if entry
        .message
        .get("from")
        .and_then(serde_json::Value::as_str)
        != Some(entry.sender)
        || entry
            .message
            .get("project_slug")
            .and_then(serde_json::Value::as_str)
            != Some(archive.slug.as_str())
    {
        return Err(invalid(
            "archive reconciliation identity does not match its project/sender",
        ));
    }
    if entry.body_md.len() > MAX_MESSAGE_ARTIFACT_BYTES {
        return Err(invalid(
            "archive reconciliation message byte budget exceeded",
        ));
    }
    if entry.recipients.len() > MAX_RECIPIENTS {
        return Err(invalid("archive reconciliation recipient budget exceeded"));
    }
    let mut recipients = Vec::new();
    for kind in ["to", "cc", "bcc"] {
        let names = entry
            .message
            .get(kind)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                invalid(format!(
                    "archive reconciliation requires an explicit {kind} array"
                ))
            })?;
        for name in names {
            let name = name
                .as_str()
                .ok_or_else(|| invalid("recipient must be a string"))?;
            crate::validate_archive_component("recipient", name)?;
            recipients.push(name.to_string());
            if recipients.len() > MAX_RECIPIENTS {
                return Err(invalid("archive reconciliation recipient budget exceeded"));
            }
        }
    }
    recipients.sort_unstable();
    recipients.dedup();
    let mut supplied = entry.recipients.to_vec();
    supplied.sort_unstable();
    supplied.dedup();
    if recipients != supplied || recipients.is_empty() {
        return Err(invalid(
            "archive reconciliation recipient routing does not match frontmatter",
        ));
    }
    if !entry.extra_paths.is_empty() {
        return Err(invalid(
            "archive reconciliation does not authorize extra commit paths",
        ));
    }
    let full = crate::render_message_bundle_content(entry.message, entry.body_md)?;
    if full.len() > MAX_MESSAGE_ARTIFACT_BYTES {
        return Err(invalid(
            "archive reconciliation message byte budget exceeded",
        ));
    }
    let inbox = crate::render_message_bundle_content(
        &crate::redact_message_bcc_for_inbox(entry.message),
        entry.body_md,
    )?;
    let bytes = full
        .len()
        .checked_mul(2)
        .and_then(|size| {
            inbox
                .len()
                .checked_mul(recipients.len())
                .and_then(|n| size.checked_add(n))
        })
        .ok_or_else(|| invalid("archive reconciliation bundle size overflow"))?;
    if bytes > MAX_BUNDLE_BYTES {
        return Err(invalid(
            "archive reconciliation bundle byte budget exceeded",
        ));
    }
    let (paths, _, _) =
        crate::message_paths_for_bundle(archive, entry.message, entry.sender, &recipients)?;
    let mut targets = vec![
        (paths.canonical.clone(), full.as_bytes()),
        (paths.outbox, full.as_bytes()),
    ];
    targets.extend(paths.inbox.into_iter().map(|path| (path, inbox.as_bytes())));
    let repo_root = crate::archive_repo_root_checked(archive)?;
    let rel_paths = targets
        .iter()
        .map(|(path, _)| crate::rel_path_cached(&archive.canonical_repo_root, path))
        .collect::<crate::Result<Vec<_>>>()?;
    let repo = Repository::open(repo_root)?;
    let expected_oids = targets
        .iter()
        .map(|(_, bytes)| Oid::hash_object(ObjectType::Blob, bytes))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let files_created = crate::with_project_lock(archive, || {
        // Missing working-tree copies do not erase the Git ledger's authority.
        // Check every committed copy before publishing even the first repair;
        // otherwise a different reply parent or extension field could silently
        // replace history that SQLite cannot reconstruct independently.
        reject_committed_message_conflicts(&repo, &rel_paths, &expected_oids)?;
        // Inspect every destination before publishing any repair. In particular,
        // a conflicting inbox must not be silently overwritten after repairing
        // the canonical file. The publication primitive rechecks collisions.
        let missing = targets
            .iter()
            .map(|(path, expected)| artifact_missing(path, expected))
            .collect::<crate::Result<Vec<_>>>()?;
        if missing.iter().any(|missing| *missing) {
            crate::reject_canonical_message_id_collision(
                archive,
                entry.message,
                &paths.canonical,
                &full,
            )?;
        }
        let mut created = 0;
        for ((path, expected), missing) in targets.iter().zip(missing) {
            if missing {
                publish_missing_file(path, expected)?;
                created += 1;
            }
        }
        Ok(created)
    })?;

    let git_commit_needed = !head_contains(&repo, &rel_paths, &expected_oids)?;
    if git_commit_needed {
        let refs: Vec<&str> = rel_paths.iter().map(String::as_str).collect();
        crate::commit_paths_with_retry(
            repo_root,
            config,
            &format!("repair: reconcile message {id} archive"),
            &refs,
        )?;
        // Open afresh: a queued/coalesced commit or an old cached HEAD cannot
        // substitute for evidence that these exact blobs reached Git.
        let verified = Repository::open(repo_root)?;
        if !head_contains(&verified, &rel_paths, &expected_oids)? {
            return Err(invalid(
                "archive reconciliation Git verification failed; files retained",
            ));
        }
    }
    Ok(ReconcileResult {
        files_created,
        git_commit_needed,
    })
}

/// A repair may fill a missing Git path or restore an object whose expected
/// identity is already recorded. It must not replace a different committed
/// payload, even when every working-tree copy is missing or agrees with the
/// proposed replacement. This inspects only the bundle's paths, not the whole
/// archive, and pins one tree for all copies in this preflight.
fn reject_committed_message_conflicts(
    repo: &Repository,
    paths: &[String],
    oids: &[Oid],
) -> crate::Result<()> {
    if paths.len() != oids.len() {
        return Err(invalid("archive verification path/object count mismatch"));
    }
    let head = match repo.head() {
        Ok(head) => head,
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let tree = head.peel_to_tree()?;
    for (path, expected) in paths.iter().zip(oids) {
        let entry = match tree.get_path(Path::new(path)) {
            Ok(entry) => entry,
            Err(error) if error.code() == ErrorCode::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if entry.kind() != Some(ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
            || entry.id() != *expected
        {
            return Err(invalid(format!(
                "committed archive artifact {path} conflicts with authoritative message; preserved"
            )));
        }
    }
    Ok(())
}

fn head_contains(repo: &Repository, paths: &[String], oids: &[Oid]) -> crate::Result<bool> {
    if paths.len() != oids.len() {
        return Err(invalid("archive verification path/object count mismatch"));
    }
    let head = match repo.head() {
        Ok(head) => head,
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let tree = head.peel_to_tree()?;
    let odb = repo.odb()?;
    let mut checked = HashSet::new();
    for (path, expected) in paths.iter().zip(oids) {
        match tree.get_path(Path::new(path)) {
            Ok(entry)
                if entry.kind() == Some(ObjectType::Blob)
                    && matches!(entry.filemode(), 0o100644 | 0o100755)
                    && entry.id() == *expected => {}
            Ok(_) => return Ok(false),
            Err(error) if error.code() == ErrorCode::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        if checked.insert(*expected) {
            // A tree can name a missing/corrupt blob. Verify actual object
            // contents, bounded before loading, not just its advertised ID.
            let (size, kind) = match odb.read_header(*expected) {
                Ok(header) => header,
                Err(error) if error.code() == ErrorCode::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if kind != ObjectType::Blob || size > MAX_MESSAGE_ARTIFACT_BYTES {
                return Ok(false);
            }
            let blob = repo.find_blob(*expected)?;
            if Oid::hash_object(ObjectType::Blob, blob.content())? != *expected {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn artifact_missing(path: &Path, expected: &[u8]) -> crate::Result<bool> {
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid(
            "archive reconciliation refuses a symlinked artifact authority",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid("archive artifact is not a regular file"));
        }
        Ok(metadata) if metadata.len() != expected.len() as u64 => {
            return Err(invalid(
                "archive artifact conflicts with authoritative message; preserved",
            ));
        }
        Ok(_) => {}
    }
    let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(path)?;
    let mut observed = Vec::new();
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut observed)?;
    if observed != expected {
        return Err(invalid(
            "archive artifact conflicts with authoritative message; preserved",
        ));
    }
    Ok(false)
}

fn publish_missing_file(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    crate::ensure_parent_dir(path)?;
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid(
            "archive reconciliation authority changed before publication",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("archive artifact has no parent"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".message-reconcile-")
        .tempfile_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(file) => {
            file.sync_all()?;
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        }
        Err(error) => {
            // Preserve the candidate too: a raced-in destination is never
            // replaced, and the error does not erase either piece of evidence.
            let kind = error.error.kind();
            let kept = error.file.keep().map_err(|error| error.error)?;
            drop(kept.0);
            Err(std::io::Error::new(
                kind,
                format!(
                    "archive publication refused; candidate retained at {}",
                    kept.1.display(),
                ),
            )
            .into())
        }
    }
}

/// Read a surviving canonical/outbox artifact with a bounded no-follow open.
/// Used by the DB reconciler to retain archive-only metadata such as `reply_to`.
pub fn read_surviving_message(path: &Path) -> crate::Result<Option<(serde_json::Value, String)>> {
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid("archive recovery source has a symlinked authority"));
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata)
            if !metadata.is_file() || metadata.len() > MAX_MESSAGE_ARTIFACT_BYTES as u64 =>
        {
            return Err(invalid(
                "archive recovery source is nonregular or exceeds its byte bound",
            ));
        }
        Ok(_) => {}
    }
    let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_MESSAGE_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MESSAGE_ARTIFACT_BYTES {
        return Err(invalid("archive recovery source grew past its byte bound"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("archive recovery source is not UTF-8"))?;
    let (frontmatter, body) = text
        .strip_prefix("---json\n")
        .and_then(|text| text.split_once("\n---\n\n"))
        .ok_or_else(|| invalid("archive recovery source has invalid canonical frontmatter"))?;
    let message = serde_json::from_str(frontmatter)?;
    Ok(Some((message, body.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture() -> (
        tempfile::TempDir,
        Config,
        ProjectArchive,
        serde_json::Value,
        Vec<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: dir.path().to_path_buf(),
            ..Config::default()
        };
        let archive = crate::ensure_archive(&config, "reconcile-project").unwrap();
        let message = json!({
            "id": 42, "from": "BlueLake", "to": ["GreenStone"], "cc": [], "bcc": ["RedFox"],
            "subject": "Durable handoff", "created": "2026-09-17T01:02:03.123456Z",
            "project": "/test/reconcile", "project_slug": "reconcile-project", "thread_id": "thread-1",
            "reply_to": 7, "topic": "work", "importance": "high", "ack_required": true, "attachments": [],
        });
        (
            dir,
            config,
            archive,
            message,
            vec!["GreenStone".into(), "RedFox".into()],
        )
    }

    fn entry<'a>(
        message: &'a serde_json::Value,
        recipients: &'a [String],
    ) -> MessageBundleBatchEntry<'a> {
        MessageBundleBatchEntry {
            message,
            body_md: "Keep Unicode: λ\n\n",
            sender: "BlueLake",
            recipients,
            extra_paths: &[],
        }
    }

    #[test]
    fn repair_materializes_and_commits_exact_copies_without_redelivery() {
        let (_dir, config, archive, message, recipients) = fixture();
        let first =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(first.files_created, 4);
        assert!(first.git_commit_needed);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let canonical = read_surviving_message(&paths.canonical).unwrap().unwrap();
        assert_eq!(canonical.0, message);
        assert_eq!(canonical.1, "Keep Unicode: λ\n\n");
        assert_eq!(
            read_surviving_message(&paths.outbox).unwrap().unwrap(),
            canonical
        );
        for inbox in paths.inbox {
            let (redacted, body) = read_surviving_message(&inbox).unwrap().unwrap();
            assert_eq!(redacted["bcc"], json!([]));
            assert_eq!(redacted["reply_to"], 7);
            assert_eq!(body, canonical.1);
        }
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        let second =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(second, ReconcileResult::default());
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }

    #[test]
    fn repair_rejects_any_conflicting_copy_before_creating_missing_files() {
        let (_dir, config, archive, message, recipients) = fixture();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        crate::ensure_parent_dir(&paths.outbox).unwrap();
        std::fs::write(&paths.outbox, b"preserve conflicting evidence").unwrap();
        assert!(reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err());
        assert!(!paths.canonical.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
        assert_eq!(
            std::fs::read(&paths.outbox).unwrap(),
            b"preserve conflicting evidence"
        );
    }

    #[test]
    fn repair_existing_uncommitted_files_verifies_git_instead_of_skipping() {
        let (_dir, config, archive, message, recipients) = fixture();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let full =
            crate::render_message_bundle_content(&message, entry(&message, &recipients).body_md)
                .unwrap();
        crate::ensure_parent_dir(&paths.canonical).unwrap();
        std::fs::write(&paths.canonical, full.as_bytes()).unwrap();
        let repaired =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(repaired.files_created, 3);
        assert!(repaired.git_commit_needed);
        assert_eq!(std::fs::read(&paths.canonical).unwrap(), full.as_bytes());
    }

    #[test]
    fn repair_rejects_committed_conflicts_before_publishing_any_missing_copy() {
        for copy in ["canonical", "outbox", "inbox"] {
            let (_dir, config, archive, message, recipients) = fixture();
            let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                .unwrap()
                .0;
            let target = match copy {
                "canonical" => &paths.canonical,
                "outbox" => &paths.outbox,
                _ => &paths.inbox[0],
            };
            let mut original = message.clone();
            original["reply_to"] = json!(9);
            if copy == "inbox" {
                original = crate::redact_message_bcc_for_inbox(&original);
            }
            let bytes = crate::render_message_bundle_content(
                &original,
                entry(&message, &recipients).body_md,
            )
            .unwrap();
            crate::ensure_parent_dir(target).unwrap();
            std::fs::write(target, &bytes).unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, target).unwrap();
            crate::commit_paths_with_retry(
                &archive.repo_root,
                &config,
                "fixture: retain original reply metadata",
                &[relative.as_str()],
            )
            .unwrap();
            // Preserve the on-disk evidence outside the bundle rather than
            // deleting it. Every expected destination is now missing.
            let evidence = config.storage_root.join("original-message-evidence.md");
            std::fs::rename(target, &evidence).unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let before = repo.head().unwrap().target().unwrap();

            let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
                .unwrap_err();
            assert!(
                error.to_string().contains("committed archive artifact"),
                "{copy}: {error}"
            );
            assert!(!paths.canonical.exists(), "{copy}");
            assert!(!paths.outbox.exists(), "{copy}");
            assert!(paths.inbox.iter().all(|path| !path.exists()), "{copy}");
            assert_eq!(std::fs::read(&evidence).unwrap(), bytes.as_bytes());
            assert_eq!(repo.head().unwrap().target().unwrap(), before);
        }
    }

    #[test]
    fn repair_rejects_working_tree_consensus_that_disagrees_with_git() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        // SQLite has no immediate reply-parent column. All working copies can
        // agree with a proposed payload while contradicting the durable one.
        message["reply_to"] = json!(9);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let full =
            crate::render_message_bundle_content(&message, entry(&message, &recipients).body_md)
                .unwrap();
        let inbox = crate::render_message_bundle_content(
            &crate::redact_message_bcc_for_inbox(&message),
            entry(&message, &recipients).body_md,
        )
        .unwrap();
        std::fs::write(&paths.canonical, &full).unwrap();
        std::fs::write(&paths.outbox, &full).unwrap();
        for path in &paths.inbox {
            std::fs::write(path, &inbox).unwrap();
        }

        let error = reconcile_message_bundle(&archive, &config, entry(&message, &recipients))
            .unwrap_err();
        assert!(error.to_string().contains("committed archive artifact"));
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
        assert_eq!(std::fs::read(&paths.canonical).unwrap(), full.as_bytes());
        assert_eq!(std::fs::read(&paths.outbox).unwrap(), full.as_bytes());
        for path in &paths.inbox {
            assert_eq!(std::fs::read(path).unwrap(), inbox.as_bytes());
        }
    }

    #[test]
    fn repair_restores_missing_working_copies_from_matching_committed_identity() {
        let (_dir, config, archive, message, recipients) = fixture();
        reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        let mut copies = vec![paths.canonical, paths.outbox];
        copies.extend(paths.inbox);
        let mut saved = Vec::new();
        for (index, path) in copies.iter().enumerate() {
            saved.push(std::fs::read(path).unwrap());
            std::fs::rename(
                path,
                config.storage_root.join(format!("evidence-{index}.md")),
            )
            .unwrap();
        }

        let result =
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
        assert_eq!(result.files_created, copies.len());
        assert!(!result.git_commit_needed);
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
        for (path, expected) in copies.iter().zip(saved) {
            assert_eq!(std::fs::read(path).unwrap(), expected);
        }
    }

    #[test]
    fn publication_collision_preserves_both_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("occupied.md");
        std::fs::write(&path, b"sentinel").unwrap();
        assert!(publish_missing_file(&path, b"candidate").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
        let candidates: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".message-reconcile-")
            })
            .collect();
        assert_eq!(candidates.len(), 1);
        assert_eq!(std::fs::read(&candidates[0]).unwrap(), b"candidate");
    }

    #[cfg(unix)]
    #[test]
    fn repair_refuses_dangling_symlink_without_touching_destination() {
        let (_dir, config, archive, message, recipients) = fixture();
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        crate::ensure_parent_dir(&paths.canonical).unwrap();
        let outside = config.storage_root.join("outside-missing");
        std::os::unix::fs::symlink(&outside, &paths.canonical).unwrap();
        assert!(reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).is_err());
        assert!(!outside.exists());
        assert_eq!(std::fs::read_link(&paths.canonical).unwrap(), outside);
    }

    #[test]
    fn repair_requires_original_identity_and_timestamp() {
        let (_dir, config, archive, message, recipients) = fixture();
        for (key, bad) in [
            ("id", json!(0)),
            ("created", json!("bad")),
            ("from", json!("RedFox")),
            ("project_slug", json!("other")),
            ("bcc", json!(null)),
        ] {
            let mut invalid = message.clone();
            invalid[key] = bad;
            assert!(
                reconcile_message_bundle(&archive, &config, entry(&invalid, &recipients)).is_err(),
                "{key}"
            );
        }
        assert!(
            reconcile_message_bundle(&archive, &config, entry(&message, &recipients[..1])).is_err()
        );
    }

    #[test]
    fn head_verification_rejects_a_tree_that_names_a_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let bytes = b"authoritative but not stored yet";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        // A raw tree object can refer to an absent blob; creating it does not
        // require deleting or corrupting any previously valid evidence.
        let mut tree_bytes = b"100644 message.md\0".to_vec();
        tree_bytes.extend_from_slice(oid.as_bytes());
        let tree_id = repo
            .odb()
            .unwrap()
            .write(ObjectType::Tree, &tree_bytes)
            .unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("reconcile-test", "reconcile@local").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "incomplete object graph",
            &tree,
            &[],
        )
        .unwrap();
        let paths = vec!["message.md".to_string()];
        assert!(!head_contains(&repo, &paths, &[oid]).unwrap());
        assert_eq!(repo.blob(bytes).unwrap(), oid);
        assert!(head_contains(&repo, &paths, &[oid]).unwrap());
    }
}
