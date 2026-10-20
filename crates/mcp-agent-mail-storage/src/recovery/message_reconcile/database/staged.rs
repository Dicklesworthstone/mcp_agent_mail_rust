//! Recover archive-only metadata staged before a failed Git commit.
//!
//! The index is a last-resort surviving copy, never a replacement for live DB
//! identity/routing or committed evidence. Read one bounded private snapshot;
//! never refresh, repair, or write the real index while selecting metadata.

use std::io::Write;
use std::path::{Path, PathBuf};

use git2::{ErrorCode, Index, ObjectType, Oid, Repository};
use serde_json::Value;

use super::{
    PreparedMessage, ReconcileFailure, merge_surviving_metadata, restore_inbox_metadata,
    validate_surviving_message,
};
use crate::ProjectArchive;

const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
// Recipient copies and other projects must not consume this project's
// canonical scan allowance. Native decoding is bounded separately by bytes.
const MAX_INDEX_ENTRIES: u32 = 100_000;

fn index_io_failure(context: &str, error: std::io::Error) -> ReconcileFailure {
    // Classify the typed error before adding context. A String would turn
    // ENOSPC/EDQUOT into conflicting evidence and discard this message's retry
    // ID, even though none of its staged metadata was disproved.
    let mut failure = ReconcileFailure::from(crate::StorageError::Io(error));
    failure.detail = format!("{context}: {}", failure.detail);
    failure
}

fn write_index_snapshot(writer: &mut impl Write, bytes: &[u8]) -> Result<(), ReconcileFailure> {
    writer
        .write_all(bytes)
        .map_err(|error| index_io_failure("staged archive index snapshot write failed", error))
}

struct StagedMessages {
    index: Index,
    // Keep the private snapshot alive until libgit2 has released the index.
    _snapshot: tempfile::NamedTempFile,
}

impl StagedMessages {
    fn open(repo: &Repository) -> Result<Option<Self>, ReconcileFailure> {
        let path = repo.path().join("index");
        if crate::path_existing_prefix_has_symlink(&path).map_err(|error| error.to_string())? {
            return Err("staged archive index has a symlinked authority"
                .to_string()
                .into());
        }
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &path,
            MAX_INDEX_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(index_io_failure(
                    "staged archive index is unavailable",
                    error,
                ));
            }
        };
        let header = bytes
            .get(..12)
            .ok_or_else(|| "staged archive index has an incomplete header".to_string())?;
        if &header[..4] != b"DIRC" {
            return Err("staged archive index has an invalid signature"
                .to_string()
                .into());
        }
        let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        if !matches!(version, 2..=4) {
            return Err("staged archive index has an unsupported version"
                .to_string()
                .into());
        }
        let count = usize::try_from(u32::from_be_bytes([
            header[8], header[9], header[10], header[11],
        ]))
        .map_err(|_| "staged archive index entry count overflow".to_string())?;
        // SHA-1 index v2/v3 entries occupy at least 64 encoded bytes; v4
        // needs 62 fixed bytes plus a prefix varint and NUL. SHA-256 entries
        // are longer. The admitted input, not unrelated recipient fanout,
        // bounds native allocation even when a tiny header forges its count.
        if count > bytes.len().saturating_sub(12) / 64 {
            return Err("staged archive index decode entry budget exceeded"
                .to_string()
                .into());
        }
        let mut snapshot = tempfile::Builder::new()
            .prefix(".message-reconcile-index-")
            .tempfile()
            .map_err(|error| {
                index_io_failure("staged archive index snapshot creation failed", error)
            })?;
        write_index_snapshot(&mut snapshot, &bytes)?;
        drop(bytes);
        // Unsupported/corrupt formats (including an unavailable split index)
        // are errors, not an empty index or permission to invent reply fields.
        let index = Index::open_ext(snapshot.path(), repo.object_format())
            .map_err(|error| format!("staged archive index cannot be decoded: {error}"))?;
        if index.len() != count {
            return Err("staged archive index decoded entry count changed"
                .to_string()
                .into());
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
        entries_left: &mut u32,
    ) -> Result<(), String> {
        let root = crate::archive_project_root_checked(archive)
            .map_err(|error| error.to_string())?
            .join("messages");
        let prefix = format!(
            "{}/",
            crate::rel_path_cached(&archive.canonical_repo_root, &root)
                .map_err(|error| error.to_string())?,
        );
        let canonical_relative = crate::rel_path_cached(&archive.canonical_repo_root, canonical)
            .map_err(|error| error.to_string())?;
        if !canonical_relative.starts_with(&prefix) {
            return Err("staged canonical target is outside its project namespace".to_string());
        }
        // The private index uses Git's case-sensitive sorted path order.
        // Seek once; unrelated projects and inbox/outbox copies do not spend
        // canonical enumeration or object-read budgets. This selects metadata,
        // not publication authority: the later mandatory identity preflight
        // still checks HEAD, all target ancestors, and staged obstructions.
        let start = match self.index.find_prefix(prefix.as_str()) {
            Ok(start) => start,
            Err(error) if error.code() == ErrorCode::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        for position in start..self.index.len() {
            let entry = self
                .index
                .get(position)
                .ok_or_else(|| "staged canonical scan lost its index entry".to_string())?;
            let Some(suffix) = entry.path.strip_prefix(prefix.as_bytes()) else {
                break;
            };
            // Charge every entry in the selected namespace, even non-message
            // files. Canonical blobs still share the metadata byte budget.
            *entries_left = entries_left
                .checked_sub(1)
                .ok_or_else(|| "staged canonical scan entry budget exceeded".to_string())?;
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
) -> Result<Option<Value>, ReconcileFailure> {
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
        let mut entries_left = MAX_INDEX_ENTRIES;
        snapshot.reject_other_canonical_id(
            repo,
            archive,
            canonical,
            id,
            &mut remaining,
            &mut entries_left,
        )?;
    }
    Ok(surviving)
}

#[cfg(test)]
mod tests {
    use super::super::tests::git_fixture;
    use super::super::{ReconcileResult, read_surviving_message, reconcile_prepared};
    use super::*;
    use serde_json::json;

    #[test]
    fn staged_index_pressure_uses_typed_errors_not_context_or_error_text() {
        use std::io::{Error, ErrorKind};

        for (kind, transient) in [
            (ErrorKind::StorageFull, true),
            (ErrorKind::QuotaExceeded, true),
            (ErrorKind::OutOfMemory, true),
            (ErrorKind::ResourceBusy, true),
            (ErrorKind::WouldBlock, true),
            (ErrorKind::TimedOut, true),
            (ErrorKind::PermissionDenied, false),
            (ErrorKind::Interrupted, false),
            (ErrorKind::InvalidData, false),
            (ErrorKind::NotFound, false),
            (ErrorKind::Other, false),
        ] {
            for context in [
                "staged index read",
                "staged snapshot creation",
                "staged snapshot write",
            ] {
                let failure =
                    index_io_failure(context, Error::new(kind, "disk full; quota exceeded"));
                assert_eq!(failure.transient, transient, "{kind:?}: {failure:?}");
                assert!(failure.detail.starts_with(context));
                // The old String boundary discarded even a typed pressure error.
                assert!(!ReconcileFailure::from(failure.detail).transient);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actual_snapshot_enospc_retains_staged_reply_until_live_retry_commits_it() {
        use super::super::{
            ReconcileCursor, ReconcileReport, outcome, prepare_message, reconcile_message_batch,
            record_failure,
        };
        use fastmcp_core::block_on;
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;

        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET archive_metadata_json = NULL WHERE id = 901")
                .unwrap();
            drop(conn);
            let original = prepare_message(cx, pool, 901).unwrap();
            assert!(!original.archive_metadata_known);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let mut full = original.message.clone();
            full["reply_to"] = json!(700);
            full["future_metadata"] = json!({"retain": "index-only extension"});
            let inbox = crate::redact_message_bcc_for_inbox(&full);
            let evidence = config.storage_root.join("retained-enospc-inbox.md");
            let bytes = stage_and_retain(
                &repo,
                &archive,
                &paths.inbox[0],
                &inbox,
                &original.body,
                &evidence,
            );
            let index_before = std::fs::read(repo.path().join("index")).unwrap();
            let before = mailbox_evidence(cx, pool);
            let cursor_past_message = || ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(901),
                backfill_ceiling: Some(900),
                startup_backfill_complete: true,
                next_lane_is_retry: true,
                ..Default::default()
            };
            let stop = AtomicBool::new(false);
            let without_retry =
                reconcile_message_batch(cx, pool, config, &mut cursor_past_message(), &stop)
                    .unwrap();
            assert_eq!((without_retry.scanned, without_retry.repaired), (0, 0));

            // The actual production snapshot-write helper receives the kernel's
            // ENOSPC, not a fabricated successful index or archive result. The
            // live archive filesystem is not filled by this fixture.
            let mut full_device = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let failure = write_index_snapshot(&mut full_device, &index_before).unwrap_err();
            assert!(failure.transient, "{failure:?}");
            assert!(
                failure
                    .detail
                    .contains("staged archive index snapshot write failed")
            );
            let mut cursor = cursor_past_message();
            let mut failed = ReconcileReport::default();
            record_failure(901, failure, &mut cursor, &mut failed, Instant::now());
            assert_eq!(
                (failed.deferred_transient, failed.deferred_conflict),
                (1, 0)
            );
            assert_eq!(cursor.retries.front().unwrap().id, 901);
            assert_eq!(
                std::fs::read(repo.path().join("index")).unwrap(),
                index_before
            );
            let cancelled =
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(true))
                    .unwrap();
            assert!(cancelled.interrupted);
            assert_eq!(cursor.retries.len(), 1);

            let repaired = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (
                    repaired.repaired,
                    repaired.files_created,
                    repaired.retry_len,
                    repaired.deferred
                ),
                (1, 4, 0, 0)
            );
            assert_eq!(mailbox_evidence(cx, pool), before);
            let verified = Repository::open(&archive.repo_root).unwrap();
            let tree = verified.head().unwrap().peel_to_tree().unwrap();
            for (path, expected) in [(&paths.canonical, &full), (&paths.outbox, &full)]
                .into_iter()
                .chain(paths.inbox.iter().map(|path| (path, &inbox)))
            {
                let (message, body) = read_surviving_message(path).unwrap().unwrap();
                assert_eq!(&message, expected);
                assert_eq!(body, original.body);
                let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
                let entry = tree.get_path(Path::new(&relative)).unwrap();
                assert_eq!(
                    verified.find_blob(entry.id()).unwrap().content(),
                    std::fs::read(path).unwrap()
                );
            }
            assert_eq!(inbox["bcc"], json!([]));
            assert_eq!(full["bcc"], json!(["RedFox"]));
            assert_eq!(std::fs::read(evidence).unwrap(), bytes);
            let head = verified.head().unwrap().target();
            let again = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!((again.repaired, again.deferred), (0, 0));
            assert_eq!(verified.head().unwrap().target(), head);
            assert_eq!(mailbox_evidence(cx, pool), before);
        });
    }

    #[test]
    fn missing_index_is_absence_but_invalid_index_does_not_become_a_pressure_retry() {
        let (_temp, config, _original, archive) = git_fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let path = repo.path().join("index");
        if path.exists() {
            std::fs::rename(
                &path,
                config.storage_root.join("retained-before-invalid-index"),
            )
            .unwrap();
        }
        assert!(StagedMessages::open(&repo).unwrap().is_none());
        assert!(!path.exists());
        let malformed = b"DIRC\0\0\0\x02\xff\xff\xff\xffdisk full; quota exceeded";
        std::fs::write(&path, malformed).unwrap();
        let error = StagedMessages::open(&repo).err().unwrap();
        assert!(
            !error.transient,
            "malformed authority is not pressure: {error:?}"
        );
        assert!(error.detail.contains("entry budget"));
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
    }

    fn add_recipient_index_entries(repo: &Repository, archive: &ProjectArchive, source: &Path) {
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, source).unwrap();
        let mut index = repo.index().unwrap();
        let mut entry = index.get_path(Path::new(&relative), 0).unwrap();
        // Real Git index entries, not 100,001 working-tree files. Their path
        // population must not prevent recovery of a small canonical namespace.
        for number in 0..100_001 {
            entry.path =
                format!("projects/other/agents/BlueLake/inbox/2026/10/{number:06}.md").into_bytes();
            index.add(&entry).unwrap();
        }
        index.write().unwrap();
        assert!(index.len() > usize::try_from(MAX_INDEX_ENTRIES).unwrap());
        assert!(std::fs::metadata(repo.path().join("index")).unwrap().len() <= MAX_INDEX_BYTES);
    }

    fn mailbox_evidence(cx: &asupersync::Cx, pool: &mcp_agent_mail_db::DbPool) -> Value {
        let conn = super::super::outcome(fastmcp_core::block_on(pool.acquire(cx))).unwrap();
        let messages = conn
            .query_sync(
                "SELECT id, body_md, archive_metadata_json FROM messages ORDER BY id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| {
                json!([
                    row.get_named::<i64>("id").unwrap(),
                    row.get_named::<String>("body_md").unwrap(),
                    row.get_named::<Option<String>>("archive_metadata_json")
                        .unwrap(),
                ])
            })
            .collect::<Vec<_>>();
        let receipts = conn.query_sync(
            "SELECT message_id, agent_id, kind, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id", &[],
        ).unwrap().iter().map(|row| json!([
            row.get_named::<i64>("message_id").unwrap(),
            row.get_named::<i64>("agent_id").unwrap(),
            row.get_named::<String>("kind").unwrap(),
            row.get_named::<Option<i64>>("read_ts").unwrap(),
            row.get_named::<Option<i64>>("ack_ts").unwrap(),
        ])).collect::<Vec<_>>();
        json!({"messages": messages, "receipts": receipts})
    }

    #[test]
    fn large_index_staged_replies_recover_through_live_mailbox_without_redelivery() {
        use super::super::{ReconcileCursor, outcome, prepare_message, reconcile_message_batch};
        use fastmcp_core::block_on;
        use std::sync::atomic::AtomicBool;

        super::super::tests::retention_fixture(3, |cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET archive_metadata_json = NULL")
                .unwrap();
            drop(conn);
            let before = mailbox_evidence(cx, pool);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let mut expected = Vec::new();
            for (id, source) in [(901, "canonical"), (902, "outbox"), (903, "inbox")] {
                let prepared = prepare_message(cx, pool, id).unwrap();
                assert!(!prepared.archive_metadata_known);
                let paths = crate::message_paths_for_bundle(
                    &archive,
                    &prepared.message,
                    &prepared.sender,
                    &prepared.recipients,
                )
                .unwrap()
                .0;
                let mut full = prepared.message.clone();
                full["reply_to"] = json!(700 + id - 901);
                full["future_metadata"] = json!({"source": source, "retain": [id, "λ"]});
                let inbox = crate::redact_message_bcc_for_inbox(&full);
                let (path, message) = match source {
                    "canonical" => (&paths.canonical, &full),
                    "outbox" => (&paths.outbox, &full),
                    _ => (&paths.inbox[0], &inbox),
                };
                let evidence = config.storage_root.join(format!("retained-{source}.md"));
                let bytes =
                    stage_and_retain(&repo, &archive, path, message, &prepared.body, &evidence);
                assert!(
                    super::super::CommittedMessages::open(&archive)
                        .unwrap()
                        .read(&archive, path)
                        .unwrap()
                        .is_none()
                );
                expected.push((prepared, full, inbox, paths, evidence, bytes));
            }
            add_recipient_index_entries(&repo, &archive, &expected[0].3.canonical);
            let index_before = std::fs::read(repo.path().join("index")).unwrap();
            let head_before = repo.head().ok().and_then(|head| head.target());
            for (prepared, full, _, paths, _, _) in &expected {
                assert_eq!(
                    read_metadata(
                        &repo,
                        &archive,
                        prepared,
                        &paths.canonical,
                        &paths.outbox,
                        &paths.inbox,
                    )
                    .unwrap()
                    .as_ref(),
                    Some(full)
                );
                assert!(!paths.canonical.exists());
                assert!(!paths.outbox.exists());
                assert!(paths.inbox.iter().all(|path| !path.exists()));
            }
            assert_eq!(
                std::fs::read(repo.path().join("index")).unwrap(),
                index_before
            );
            assert_eq!(repo.head().ok().and_then(|head| head.target()), head_before);

            let stop = AtomicBool::new(false);
            let mut cursor = ReconcileCursor::default();
            let repaired = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (repaired.repaired, repaired.files_created, repaired.deferred),
                (3, 12, 0)
            );
            assert_eq!(mailbox_evidence(cx, pool), before);
            let verified = Repository::open(&archive.repo_root).unwrap();
            let tree = verified.head().unwrap().peel_to_tree().unwrap();
            for (prepared, full, inbox, paths, evidence, bytes) in &expected {
                for (path, expected_message) in [(&paths.canonical, full), (&paths.outbox, full)]
                    .into_iter()
                    .chain(paths.inbox.iter().map(|path| (path, inbox)))
                {
                    let (message, body) = read_surviving_message(path).unwrap().unwrap();
                    assert_eq!(&message, expected_message);
                    assert_eq!(body, prepared.body);
                    let relative =
                        crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
                    let object = tree.get_path(Path::new(&relative)).unwrap();
                    assert_eq!(
                        verified.find_blob(object.id()).unwrap().content(),
                        std::fs::read(path).unwrap()
                    );
                }
                assert_eq!(full["bcc"], json!(["RedFox"]));
                assert_eq!(inbox["bcc"], json!([]));
                assert_eq!(&std::fs::read(evidence).unwrap(), bytes);
            }
            let head = verified.head().unwrap().target();
            let again = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (again.repaired, again.files_created, again.deferred),
                (0, 0, 0)
            );
            assert_eq!(verified.head().unwrap().target(), head);
            assert_eq!(mailbox_evidence(cx, pool), before);
        });
    }

    #[test]
    fn large_recipient_index_does_not_hide_a_staged_duplicate_identity() {
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
        let redacted = crate::redact_message_bcc_for_inbox(&full);
        stage_and_retain(
            &repo,
            &archive,
            &paths.inbox[0],
            &redacted,
            &original.body,
            &config.storage_root.join("retained-index-inbox.md"),
        );
        let other = archive.root.join("messages/9999/12/duplicate.md");
        let evidence = config.storage_root.join("retained-duplicate.md");
        let bytes = stage_and_retain(&repo, &archive, &other, &full, &original.body, &evidence);
        add_recipient_index_entries(&repo, &archive, &paths.inbox[0]);
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let head = repo.head().ok().and_then(|head| head.target());
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("canonical message id 9 already staged"),
            "{error}"
        );
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        assert_eq!(repo.head().ok().and_then(|head| head.target()), head);
        assert_eq!(std::fs::read(evidence).unwrap(), bytes);
    }

    #[test]
    fn scoped_metadata_scan_keeps_entry_and_blob_bounds_and_exact_prefixes() {
        let (_temp, config, original, archive) = git_fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let canonical = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0
        .canonical;
        let mut expected_bytes = 0;
        for (number, relative) in [
            "projects/project/messages/2025/01/a.md",
            "projects/project/messages/2025/01/b.md",
            "projects/project/messages-other/2025/01/c.md",
            "projects/project-other/messages/2025/01/d.md",
            "projects/Project/messages/2025/01/e.md",
        ]
        .into_iter()
        .enumerate()
        {
            let mut message = original.message.clone();
            message["id"] = if number < 2 {
                json!(77 + number)
            } else {
                json!(9)
            };
            let path = archive.repo_root.join(relative);
            let bytes = stage_and_retain(
                &repo,
                &archive,
                &path,
                &message,
                &original.body,
                &config
                    .storage_root
                    .join(format!("retained-prefix-{number}.md")),
            );
            if number < 2 {
                expected_bytes += bytes.len();
            }
        }
        let snapshot = StagedMessages::open(&repo).unwrap().unwrap();
        let before = std::fs::read(repo.path().join("index")).unwrap();
        let mut remaining = expected_bytes;
        let mut entries = 2;
        snapshot
            .reject_other_canonical_id(&repo, &archive, &canonical, 9, &mut remaining, &mut entries)
            .unwrap();
        assert_eq!((remaining, entries), (0, 0));
        for (mut bytes, mut entries) in [(expected_bytes, 1), (expected_bytes - 1, 2)] {
            let error = snapshot
                .reject_other_canonical_id(&repo, &archive, &canonical, 9, &mut bytes, &mut entries)
                .unwrap_err();
            assert!(error.contains("budget"), "{error}");
        }
        let wrong = archive
            .repo_root
            .join("projects/project-other/messages/2025/01/new.md");
        assert!(
            snapshot
                .reject_other_canonical_id(&repo, &archive, &wrong, 9, &mut expected_bytes, &mut 2)
                .unwrap_err()
                .contains("outside its project")
        );
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        assert!(!canonical.exists());
    }

    #[test]
    fn native_index_versions_preserve_metadata_and_forged_decode_counts_refuse() {
        let (_temp, config, original, archive) = git_fixture();
        let repo = Repository::open(&archive.repo_root).unwrap();
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
        stage_and_retain(
            &repo,
            &archive,
            &paths.outbox,
            &full,
            &original.body,
            &config.storage_root.join("retained-version-outbox.md"),
        );
        for version in [2, 3, 4] {
            let mut index = repo.index().unwrap();
            index.set_version(version).unwrap();
            index.write().unwrap();
            let before = std::fs::read(repo.path().join("index")).unwrap();
            assert_eq!(
                read_metadata(
                    &repo,
                    &archive,
                    &original,
                    &paths.canonical,
                    &paths.outbox,
                    &paths.inbox
                )
                .unwrap(),
                Some(full.clone())
            );
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), before);
        }
        let path = repo.path().join("index");
        std::fs::rename(&path, config.storage_root.join("retained-version-index")).unwrap();
        for version in [2_u32, 3, 4, 5] {
            let mut forged = b"DIRC".to_vec();
            forged.extend_from_slice(&version.to_be_bytes());
            forged.extend_from_slice(&u32::MAX.to_be_bytes());
            std::fs::write(&path, &forged).unwrap();
            let error = StagedMessages::open(&repo).err().unwrap();
            assert!(
                error.detail.contains(if version == 5 {
                    "unsupported version"
                } else {
                    "budget"
                }),
                "{error:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), forged);
        }
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
    }

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
