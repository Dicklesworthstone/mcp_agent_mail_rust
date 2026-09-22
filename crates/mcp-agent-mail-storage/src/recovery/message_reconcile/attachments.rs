//! Recover attachment bytes without re-running a send or an image conversion.
//!
//! Attachment paths must belong to this project's archive. One pinned Git tree
//! supplies committed bytes. Uncommitted raw files and retained originals may
//! instead be witnessed by their original-content digest. Converted WebP files
//! need their exact-content SHA256; the original-image digest is not authority
//! for encoded bytes. The caller
//! preflights every destination and commits all witnessed files with the mail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use git2::{ErrorCode, ObjectType, Oid, Repository};
use serde_json::Value;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;

use super::invalid;
use crate::ProjectArchive;

const MAX_ATTACHMENT_RECORDS: usize = 128;
const MAX_ATTACHMENT_PATH_BYTES: usize = 4096;

#[derive(Default)]
struct ExpectedFile {
    size: Option<u64>,
    // Image metadata hashes the ORIGINAL, not the converted WebP. Only raw
    // files and retained originals can be checked against this digest.
    source_sha1: Option<String>,
    // The exact stored file's digest, independent of original-image identity.
    content_sha256: Option<String>,
}

fn optional_path<'a>(attachment: &'a Value, field: &str) -> crate::Result<Option<&'a str>> {
    match attachment.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) => Ok(Some(path)),
        Some(_) => Err(invalid(format!("attachment {field} is not a path string"))),
    }
}

fn source_sha1(attachment: &Value) -> crate::Result<String> {
    let digest = attachment
        .get("sha1")
        .and_then(Value::as_str)
        .filter(|digest| {
            digest.len() == 40
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| invalid("attachment has no valid original-content SHA1"))?;
    Ok(digest.to_string())
}

fn content_sha256(attachment: &Value) -> crate::Result<Option<String>> {
    match attachment.get("content_sha256") {
        None => Ok(None),
        Some(Value::String(digest))
            if digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) =>
        {
            Ok(Some(digest.clone()))
        }
        _ => Err(invalid("attachment has an invalid exact-content SHA256")),
    }
}

fn checked_path(archive: &ProjectArchive, path: &str) -> crate::Result<String> {
    let prefix = format!("projects/{}/attachments/", archive.slug);
    if path.len() > MAX_ATTACHMENT_PATH_BYTES
        || !path.starts_with(&prefix)
        || path.len() == prefix.len()
        || path.contains('\\')
        || path.contains(':')
        || path.bytes().any(|byte| byte.is_ascii_control())
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || crate::validate_repo_relative_path("recovery attachment", path)? != path
    {
        return Err(invalid(
            "recovery attachment path is not inside its project's attachment archive",
        ));
    }
    Ok(path.to_string())
}

fn add_file(
    files: &mut BTreeMap<String, ExpectedFile>,
    archive: &ProjectArchive,
    path: &str,
    expected: ExpectedFile,
) -> crate::Result<()> {
    let path = checked_path(archive, path)?;
    let previous = files.entry(path).or_default();
    if previous
        .size
        .zip(expected.size)
        .is_some_and(|(a, b)| a != b)
        || previous
            .source_sha1
            .as_ref()
            .zip(expected.source_sha1.as_ref())
            .is_some_and(|(a, b)| a != b)
        || previous
            .content_sha256
            .as_ref()
            .zip(expected.content_sha256.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        return Err(invalid(
            "duplicate attachment path has conflicting metadata",
        ));
    }
    previous.size = previous.size.or(expected.size);
    if previous.source_sha1.is_none() {
        previous.source_sha1 = expected.source_sha1;
    }
    if previous.content_sha256.is_none() {
        previous.content_sha256 = expected.content_sha256;
    }
    Ok(())
}

fn required_files(
    archive: &ProjectArchive,
    message: &Value,
) -> crate::Result<BTreeMap<String, ExpectedFile>> {
    let Some(attachments) = message.get("attachments") else {
        return Ok(BTreeMap::new());
    };
    let attachments = attachments
        .as_array()
        .ok_or_else(|| invalid("recovery attachment metadata is not an array"))?;
    if attachments.len() > MAX_ATTACHMENT_RECORDS {
        return Err(invalid("recovery attachment count budget exceeded"));
    }
    let mut files = BTreeMap::new();
    for attachment in attachments {
        match attachment.get("type").and_then(Value::as_str) {
            Some("file") => {
                let path = optional_path(attachment, "path")?
                    .ok_or_else(|| invalid("file attachment has no archive path"))?;
                let size = attachment
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("file attachment has no valid byte length"))?;
                let raw_prefix = format!("projects/{}/attachments/files/", archive.slug);
                let digest = if path.starts_with(&raw_prefix) {
                    Some(source_sha1(attachment)?)
                } else {
                    None
                };
                add_file(
                    &mut files,
                    archive,
                    path,
                    ExpectedFile {
                        size: Some(size),
                        source_sha1: digest,
                        content_sha256: content_sha256(attachment)?,
                    },
                )?;
            }
            // The message body or data_base64 owns inline bytes. Do not decode
            // them or manufacture an unreferenced WebP cache during recovery.
            Some("inline") => {}
            // External attachments are deliberately not fetched over a network.
            Some("external") => continue,
            _ => return Err(invalid("recovery attachment has an unsupported type")),
        }
        if let Some(path) = optional_path(attachment, "original_path")? {
            add_file(
                &mut files,
                archive,
                path,
                ExpectedFile {
                    size: None,
                    source_sha1: Some(source_sha1(attachment)?),
                    content_sha256: None,
                },
            )?;
        }
    }
    Ok(files)
}

/// Prepare only explicitly referenced file/original attachments. Unique bytes
/// share the caller's remaining bundle budget; duplicate references still have
/// to agree about size and original-content identity. No files are written here.
pub(super) fn prepare(
    repo: &Repository,
    archive: &ProjectArchive,
    message: &Value,
    mut remaining_bytes: usize,
) -> crate::Result<Vec<(PathBuf, Vec<u8>)>> {
    let files = required_files(archive, message)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let root = crate::archive_repo_root_checked(archive)?;
    let tree = match repo.head() {
        Ok(head) => Some(head.peel_to_tree()?),
        Err(error) if error.code() == ErrorCode::UnbornBranch => None,
        Err(error) => return Err(error.into()),
    };
    let mut prepared = Vec::with_capacity(files.len());
    for (relative, expected) in files {
        let path = root.join(&relative);
        if crate::path_existing_prefix_has_symlink(&path)? {
            return Err(invalid(
                "attachment recovery refuses a symlinked destination",
            ));
        }
        let bytes = read_attachment(
            repo,
            tree.as_ref(),
            &relative,
            &path,
            &expected,
            remaining_bytes,
        )?;
        remaining_bytes -= bytes.len();
        prepared.push((path, bytes));
    }
    Ok(prepared)
}

fn read_attachment(
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    relative: &str,
    path: &Path,
    expected: &ExpectedFile,
    remaining_bytes: usize,
) -> crate::Result<Vec<u8>> {
    let entry = match tree {
        Some(tree) => match tree.get_path(Path::new(relative)) {
            Ok(entry) => Some(entry),
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(error) => return Err(error.into()),
        },
        None => None,
    };
    let bytes = if let Some(entry) = entry {
        // A present Git entry is authoritative. Never fall back to potentially
        // different local bytes on an object error, budget failure, or conflict.
        if entry.kind() != Some(ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
        {
            return Err(invalid("committed attachment is not a regular-file blob"));
        }
        let (size, kind) = repo.odb()?.read_header(entry.id())?;
        if kind != ObjectType::Blob || size > remaining_bytes {
            return Err(invalid("attachment recovery bundle byte budget exceeded"));
        }
        let blob = repo.find_blob(entry.id())?;
        if blob.content().len() != size
            || Oid::hash_object_ext(ObjectType::Blob, blob.content(), entry.id().object_format())?
                != entry.id()
        {
            return Err(invalid(
                "attachment content does not match its Git object identity",
            ));
        }
        blob.content().to_vec()
    } else {
        // The database commit may outlive the best-effort archive commit. Raw
        // files and originals can still be proven without guessing their data.
        // New image metadata also witnesses exact encoded bytes. A legacy WebP
        // with only its source-image SHA1 must still remain deferred.
        if expected.source_sha1.is_none() && expected.content_sha256.is_none() {
            return Err(invalid(format!(
                "attachment {relative} has no committed or content-hash authority; repair deferred"
            )));
        }
        mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            path,
            remaining_bytes as u64,
        )?
    };
    if expected
        .size
        .is_some_and(|expected| expected != bytes.len() as u64)
    {
        return Err(invalid(
            "attachment byte length conflicts with its metadata",
        ));
    }
    if let Some(expected) = &expected.source_sha1
        && hex::encode(Sha1::digest(&bytes)) != *expected
    {
        return Err(invalid("attachment original-content SHA1 does not match"));
    }
    if let Some(expected) = &expected.content_sha256
        && hex::encode(Sha256::digest(&bytes)) != *expected
    {
        return Err(invalid("attachment exact-content SHA256 does not match"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::{ReconcileResult, reconcile_message_bundle};
    use super::*;
    use crate::{Config, EmbedPolicy, MessageBundleBatchEntry, StoredAttachment};
    use serde_json::json;

    fn fixture() -> (
        tempfile::TempDir,
        Config,
        ProjectArchive,
        Value,
        Vec<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: dir.path().to_path_buf(),
            keep_original_images: true,
            ..Config::default()
        };
        let archive = crate::ensure_archive(&config, "attachments-project").unwrap();
        let message = json!({
            "id": 42, "from": "BlueLake", "to": ["GreenStone"], "cc": [], "bcc": ["RedFox"],
            "subject": "Attachment handoff", "created": "2026-09-21T01:02:03.123456Z",
            "project": "/test/attachments", "project_slug": "attachments-project",
            "thread_id": null, "importance": "normal", "ack_required": true, "attachments": [],
        });
        (
            dir,
            config,
            archive,
            message,
            vec!["GreenStone".into(), "RedFox".into()],
        )
    }

    fn repair(
        archive: &ProjectArchive,
        config: &Config,
        message: &Value,
        recipients: &[String],
    ) -> crate::Result<ReconcileResult> {
        reconcile_message_bundle(
            archive,
            config,
            MessageBundleBatchEntry {
                message,
                body_md: "Keep the attached bytes.",
                sender: "BlueLake",
                recipients,
                extra_paths: &[],
            },
        )
    }

    fn commit_attachment(archive: &ProjectArchive, config: &Config, stored: &StoredAttachment) {
        let paths = stored
            .rel_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        crate::commit_paths_with_retry(&archive.repo_root, config, "fixture: attachment", &paths)
            .unwrap();
    }

    fn raw(
        archive: &ProjectArchive,
        config: &Config,
        name: &str,
        bytes: &[u8],
    ) -> StoredAttachment {
        let source = config.storage_root.join(name);
        std::fs::write(&source, bytes).unwrap();
        let stored = crate::store_raw_attachment(archive, &source, 0).unwrap();
        commit_attachment(archive, config, &stored);
        stored
    }

    #[test]
    fn raw_attachment_only_repair_restores_exact_bytes_without_a_new_commit() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let bytes = b"raw binary\x00\xff preserved";
        let stored = raw(&archive, &config, "source.bin", bytes);
        message["attachments"] = json!([stored.meta]);
        repair(&archive, &config, &message, &recipients).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        let evidence = config.storage_root.join("retained-attachment.bin");
        std::fs::rename(&path, &evidence).unwrap();
        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 1);
        assert!(!result.git_commit_needed);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&evidence).unwrap(), bytes);
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
        assert_eq!(
            repair(&archive, &config, &message, &recipients).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn real_image_and_original_recovery_does_not_hash_webp_as_original() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("source.png");
        image::RgbImage::new(2, 2).save(&source).unwrap();
        let stored =
            crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
        commit_attachment(&archive, &config, &stored);
        let webp = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        let original = archive
            .repo_root
            .join(stored.meta.original_path.as_ref().unwrap());
        let expected_webp = std::fs::read(&webp).unwrap();
        let expected_original = std::fs::read(&original).unwrap();
        assert_ne!(hex::encode(Sha1::digest(&expected_webp)), stored.meta.sha1);
        assert_eq!(
            hex::encode(Sha1::digest(&expected_original)),
            stored.meta.sha1
        );
        message["attachments"] = json!([stored.meta]);
        std::fs::rename(&webp, config.storage_root.join("retained.webp")).unwrap();
        std::fs::rename(&original, config.storage_root.join("retained.png")).unwrap();
        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 6);
        assert!(result.git_commit_needed);
        assert_eq!(std::fs::read(&webp).unwrap(), expected_webp);
        assert_eq!(std::fs::read(&original).unwrap(), expected_original);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        for path in paths.inbox {
            let (copy, _) = super::super::read_surviving_message(&path)
                .unwrap()
                .unwrap();
            assert_eq!(copy["bcc"], json!([]));
            assert_eq!(copy["attachments"], message["attachments"]);
        }
        assert_eq!(
            repair(&archive, &config, &message, &recipients).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn attachment_conflict_preflights_before_any_message_or_attachment_is_created() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let first = raw(&archive, &config, "first.bin", b"first");
        let second = raw(&archive, &config, "second.bin", b"second");
        message["attachments"] = json!([first.meta, second.meta]);
        let first_path = archive.repo_root.join(first.meta.path.as_ref().unwrap());
        let second_path = archive.repo_root.join(second.meta.path.as_ref().unwrap());
        std::fs::rename(&first_path, config.storage_root.join("retained-first.bin")).unwrap();
        std::fs::write(&second_path, b"conflicting evidence").unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        assert!(!first_path.exists());
        assert!(!archive.root.join("messages").exists());
        assert_eq!(
            std::fs::read(&second_path).unwrap(),
            b"conflicting evidence"
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }

    #[test]
    fn metadata_and_bundle_budgets_are_checked_before_publication() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let stored = raw(&archive, &config, "source.bin", b"12345");
        let repo = Repository::open(&archive.repo_root).unwrap();
        let metadata = serde_json::to_value(&stored.meta).unwrap();
        message["attachments"] = json!([metadata, metadata]);
        assert_eq!(prepare(&repo, &archive, &message, 5).unwrap().len(), 1);
        assert!(prepare(&repo, &archive, &message, 4).is_err());
        message["attachments"][1]["bytes"] = json!(6);
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        message["attachments"] = json!([metadata]);
        message["attachments"][0]["bytes"] = json!(6);
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        message["attachments"][0]["bytes"] = json!(5);
        message["attachments"][0]["sha1"] = json!("0".repeat(40));
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        message["attachments"] = json!(vec![metadata; MAX_ATTACHMENT_RECORDS + 1]);
        assert!(prepare(&repo, &archive, &message, usize::MAX).is_err());
        assert!(!archive.root.join("messages").exists());
    }

    #[test]
    fn foreign_traversal_and_missing_attachment_authority_never_create_mail() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        for path in [
            "projects/foreign/attachments/source.bin",
            "projects/attachments-project/attachments/../source.bin",
            "projects/attachments-project/attachments//source.bin",
            "projects/attachments-project/attachments/./source.bin",
            "projects/attachments-project/attachments/../../.git/config",
            "projects/attachments-project/attachments/source.bin:stream",
            "projects/attachments-project/attachments/source\0.bin",
            "projects/attachments-project/attachments/missing.bin",
        ] {
            message["attachments"] = json!([{"type": "file", "bytes": 5, "path": path}]);
            assert!(
                repair(&archive, &config, &message, &recipients).is_err(),
                "{path}"
            );
            assert!(!archive.root.join("messages").exists());
        }
        message["attachments"] = json!([
            {"type": "inline", "data_base64": "aGk="},
            {"type": "external", "url": "https://example.invalid/never-fetch"},
        ]);
        assert_eq!(
            repair(&archive, &config, &message, &recipients)
                .unwrap()
                .files_created,
            4
        );
    }

    #[test]
    fn inline_image_recovery_preserves_embedded_bytes_and_restores_its_original() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("inline.png");
        image::RgbImage::new(2, 2).save(&source).unwrap();
        let stored =
            crate::store_attachment(&archive, &config, &source, EmbedPolicy::Inline).unwrap();
        commit_attachment(&archive, &config, &stored);
        assert!(stored.meta.path.is_none());
        let original = archive
            .repo_root
            .join(stored.meta.original_path.as_ref().unwrap());
        let bytes = std::fs::read(&original).unwrap();
        std::fs::rename(&original, config.storage_root.join("retained-inline.png")).unwrap();
        message["attachments"] = json!([stored.meta]);
        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 5);
        assert_eq!(std::fs::read(&original).unwrap(), bytes);
        let paths = crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
            .unwrap()
            .0;
        let (copy, _) = super::super::read_surviving_message(&paths.canonical)
            .unwrap()
            .unwrap();
        assert_eq!(
            copy["attachments"][0]["data_base64"],
            stored.meta.data_base64.unwrap()
        );
    }

    #[test]
    fn attachment_larger_than_message_limit_still_verifies_under_bundle_limit() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let bytes = vec![42; super::super::MAX_MESSAGE_ARTIFACT_BYTES + 1];
        let stored = raw(&archive, &config, "large.bin", &bytes);
        message["attachments"] = json!([stored.meta]);
        let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        std::fs::rename(&path, config.storage_root.join("retained-large.bin")).unwrap();
        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 5);
        assert!(result.git_commit_needed);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            repair(&archive, &config, &message, &recipients).unwrap(),
            ReconcileResult::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_attachment_destinations_preserve_the_link_and_target() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let stored = raw(&archive, &config, "source.bin", b"source bytes");
        message["attachments"] = json!([stored.meta]);
        let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        let outside = config.storage_root.join("retained-outside.bin");
        std::fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        assert_eq!(std::fs::read_link(&path).unwrap(), outside);
        assert_eq!(std::fs::read(&outside).unwrap(), b"source bytes");
        assert!(!archive.root.join("messages").exists());
    }

    #[test]
    fn uncommitted_raw_attachment_is_committed_before_recovery_reports_success() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("not-yet-committed.bin");
        let bytes = b"survived the database-to-Git crash window";
        std::fs::write(&source, bytes).unwrap();
        let stored = crate::store_raw_attachment(&archive, &source, 0).unwrap();
        let relative = stored.meta.path.as_ref().unwrap();
        let path = archive.repo_root.join(relative);
        let repo = Repository::open(&archive.repo_root).unwrap();
        assert!(
            repo.head()
                .unwrap()
                .peel_to_tree()
                .unwrap()
                .get_path(Path::new(relative))
                .is_err()
        );
        message["attachments"] = json!([stored.meta]);
        assert_eq!(
            prepare(&repo, &archive, &message, bytes.len())
                .unwrap()
                .len(),
            1
        );
        assert!(prepare(&repo, &archive, &message, bytes.len() - 1).is_err());

        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(relative)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        let head = repo.head().unwrap().target().unwrap();
        assert_eq!(
            repair(&archive, &config, &message, &recipients).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
    }

    #[test]
    fn uncommitted_missing_or_modified_raw_files_do_not_authorize_repair() {
        for missing in [false, true] {
            let (_dir, config, archive, mut message, recipients) = fixture();
            let source = config.storage_root.join("source.bin");
            std::fs::write(&source, b"12345").unwrap();
            let stored = crate::store_raw_attachment(&archive, &source, 0).unwrap();
            let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
            let evidence = config.storage_root.join("retained-source.bin");
            if missing {
                std::fs::rename(&path, &evidence).unwrap();
            } else {
                // Same length is not enough to authorize an uncommitted file.
                std::fs::write(&path, b"54321").unwrap();
            }
            message["attachments"] = json!([stored.meta]);
            let repo = Repository::open(&archive.repo_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            assert!(repair(&archive, &config, &message, &recipients).is_err());
            assert!(!archive.root.join("messages").exists());
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
            assert_eq!(std::fs::read(&source).unwrap(), b"12345");
            if missing {
                assert!(!path.exists());
                assert_eq!(std::fs::read(&evidence).unwrap(), b"12345");
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), b"54321");
            }
        }
    }

    #[test]
    fn inline_original_is_hash_verified_and_committed_after_an_interrupted_send() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("uncommitted-inline.png");
        image::RgbImage::new(2, 2).save(&source).unwrap();
        let stored =
            crate::store_attachment(&archive, &config, &source, EmbedPolicy::Inline).unwrap();
        let relative = stored.meta.original_path.as_ref().unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        assert!(
            repo.head()
                .unwrap()
                .peel_to_tree()
                .unwrap()
                .get_path(Path::new(relative))
                .is_err()
        );
        message["attachments"] = json!([stored.meta]);
        let result = repair(&archive, &config, &message, &recipients).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(relative)).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), std::fs::read(&source).unwrap());
        assert_eq!(hex::encode(Sha1::digest(blob.content())), stored.meta.sha1);
    }

    #[test]
    fn uncommitted_webp_cannot_use_its_original_images_digest_as_proof() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("uncommitted-image.png");
        image::RgbImage::new(2, 2).save(&source).unwrap();
        let mut stored =
            crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
        // Legacy metadata has only the original image's digest. It must not
        // authorize a different representation of those bytes.
        stored.meta.content_sha256 = None;
        let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        let before = std::fs::read(&path).unwrap();
        message["attachments"] = json!([stored.meta]);
        let error = repair(&archive, &config, &message, &recipients).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no committed or content-hash authority")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!archive.root.join("messages").exists());
    }

    #[test]
    fn uncommitted_webp_content_digest_recovers_fresh_and_cached_images() {
        for cached in [false, true] {
            let (_dir, config, archive, mut message, recipients) = fixture();
            let source = config.storage_root.join("recoverable-image.png");
            image::RgbImage::new(2, 2).save(&source).unwrap();
            let original_bytes = std::fs::read(&source).unwrap();
            let first =
                crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
            let stored = if cached {
                let second =
                    crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
                assert_eq!(second.meta.content_sha256, first.meta.content_sha256);
                second
            } else {
                first
            };
            let webp_relative = stored.meta.path.as_ref().unwrap();
            let original_relative = stored.meta.original_path.as_ref().unwrap();
            let webp_bytes = std::fs::read(archive.repo_root.join(webp_relative)).unwrap();
            assert_eq!(
                stored.meta.content_sha256,
                Some(hex::encode(sha2::Sha256::digest(&webp_bytes)))
            );
            assert_eq!(stored.meta.sha1, hex::encode(Sha1::digest(&original_bytes)));
            assert_ne!(stored.meta.sha1, hex::encode(Sha1::digest(&webp_bytes)));
            let repo = Repository::open(&archive.repo_root).unwrap();
            let before = repo.head().unwrap().peel_to_tree().unwrap();
            for relative in [webp_relative, original_relative] {
                assert!(before.get_path(Path::new(relative)).is_err());
            }
            // Exercise the metadata round trip used to persist the accepted
            // attachment, not an in-memory recovery-only digest.
            let persisted: crate::AttachmentMeta =
                serde_json::from_str(&serde_json::to_string(&stored.meta).unwrap()).unwrap();
            message["attachments"] = json!([persisted]);
            let result = repair(&archive, &config, &message, &recipients).unwrap();
            assert_eq!(result.files_created, 4);
            assert!(result.git_commit_needed);
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            for (relative, expected) in [
                (webp_relative, &webp_bytes),
                (original_relative, &original_bytes),
            ] {
                let entry = tree.get_path(Path::new(relative)).unwrap();
                assert_eq!(
                    repo.find_blob(entry.id()).unwrap().content(),
                    expected.as_slice()
                );
                assert_eq!(
                    std::fs::read(archive.repo_root.join(relative)).unwrap(),
                    expected.as_slice()
                );
            }
            assert_eq!(std::fs::read(&source).unwrap(), original_bytes);
            let paths =
                crate::message_paths_for_bundle(&archive, &message, "BlueLake", &recipients)
                    .unwrap()
                    .0;
            for path in paths.inbox {
                let (copy, body) = super::super::read_surviving_message(&path)
                    .unwrap()
                    .unwrap();
                assert_eq!(copy["bcc"], json!([]));
                assert_eq!(copy["attachments"], message["attachments"]);
                assert_eq!(body, "Keep the attached bytes.");
            }
            let head = repo.head().unwrap().target().unwrap();
            assert_eq!(
                repair(&archive, &config, &message, &recipients).unwrap(),
                ReconcileResult::default()
            );
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        }
    }

    #[test]
    fn webp_content_digest_or_byte_conflicts_do_not_publish_recovery() {
        for (committed, corrupt_bytes) in
            [(false, true), (true, true), (false, false), (true, false)]
        {
            let (_dir, config, archive, mut message, recipients) = fixture();
            let source = config.storage_root.join("conflicting-image.png");
            image::RgbImage::new(2, 2).save(&source).unwrap();
            let original_bytes = std::fs::read(&source).unwrap();
            let stored =
                crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
            if committed {
                commit_attachment(&archive, &config, &stored);
            }
            let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
            let mut surviving_bytes = std::fs::read(&path).unwrap();
            message["attachments"] = json!([stored.meta]);
            if corrupt_bytes {
                // Identical length and original-image provenance do not prove
                // that the accepted encoded bytes survived.
                surviving_bytes[0] ^= 1;
                std::fs::write(&path, &surviving_bytes).unwrap();
            } else {
                message["attachments"][0]["content_sha256"] = json!("0".repeat(64));
            }
            let repo = Repository::open(&archive.repo_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            assert!(repair(&archive, &config, &message, &recipients).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), surviving_bytes);
            assert_eq!(std::fs::read(&source).unwrap(), original_bytes);
            assert!(!archive.root.join("messages").exists());
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        }
    }

    #[test]
    fn malformed_or_disagreeing_webp_content_digests_fail_before_publication() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let source = config.storage_root.join("digest-validation.png");
        image::RgbImage::new(2, 2).save(&source).unwrap();
        let stored =
            crate::store_attachment(&archive, &config, &source, EmbedPolicy::File).unwrap();
        let metadata = serde_json::to_value(&stored.meta).unwrap();
        let repo = Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        for malformed in [
            json!(""),
            json!("0".repeat(63)),
            json!("0".repeat(65)),
            json!("g".repeat(64)),
            json!("A".repeat(64)),
            Value::Null,
            json!(42),
            json!([]),
        ] {
            message["attachments"] = json!([metadata]);
            message["attachments"][0]["content_sha256"] = malformed;
            assert!(repair(&archive, &config, &message, &recipients).is_err());
            assert!(!archive.root.join("messages").exists());
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        }
        message["attachments"] = json!([metadata, metadata]);
        assert_eq!(
            prepare(&repo, &archive, &message, usize::MAX)
                .unwrap()
                .len(),
            2
        );
        message["attachments"][1]["content_sha256"] = json!("0".repeat(64));
        assert!(repair(&archive, &config, &message, &recipients).is_err());
        assert!(!archive.root.join("messages").exists());
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
    }

    #[test]
    fn local_hash_match_cannot_override_a_different_committed_attachment() {
        let (_dir, config, archive, mut message, recipients) = fixture();
        let mut stored = raw(&archive, &config, "committed.bin", b"before");
        let path = archive.repo_root.join(stored.meta.path.as_ref().unwrap());
        std::fs::write(&path, b"after!").unwrap();
        stored.meta.sha1 = hex::encode(Sha1::digest(b"after!"));
        message["attachments"] = json!([stored.meta]);
        let repo = Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        let error = repair(&archive, &config, &message, &recipients).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original-content SHA1 does not match")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"after!");
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        assert!(!archive.root.join("messages").exists());
    }
}
