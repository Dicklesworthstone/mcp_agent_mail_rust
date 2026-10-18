//! Decide whether a failed bundle's extra paths are already message obligations.
//!
//! This is only a bounded, filesystem-free classification. The original message
//! fingerprint still binds all attachment metadata, and normal reconciliation
//! must prove attachment hashes, bytes, paths and Git publication before clearing
//! the refusal. An arbitrary extra file is never made safe by living nearby.

use std::collections::BTreeSet;

use serde_json::Value;

const MAX_RECORDS: usize = 128;
const MAX_PATHS: usize = MAX_RECORDS * 2;
const MAX_PATH_BYTES: usize = 4096;

fn owned_path(project: &str, path: &str) -> bool {
    let prefix = format!("projects/{project}/attachments/");
    path.len() <= MAX_PATH_BYTES
        && path.starts_with(&prefix)
        && path.len() > prefix.len()
        && !path.contains('\\')
        && !path.contains(':')
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && !path.split('/').any(|part| matches!(part, "" | "." | ".."))
        && crate::validate_repo_relative_path("WBQ attachment obligation", path)
            .is_ok_and(|normalized| normalized == path)
}

/// Every extra path must occur in a field the normal attachment reconciler
/// actually verifies. External URLs and their ignored original_path fields do
/// not establish a local obligation. Extra paths may be a subset (or contain
/// duplicates); the full fingerprinted attachment set is repaired either way.
pub(super) fn covered_by_message(project: &str, message: &Value, extra: &[String]) -> bool {
    if extra.is_empty() {
        return true;
    }
    if extra.len() > MAX_PATHS
        || project.len() > 1024
        || crate::validate_archive_component("WBQ attachment project", project).is_err()
    {
        return false;
    }
    let Some(attachments) = message.get("attachments").and_then(Value::as_array) else {
        return false;
    };
    if attachments.len() > MAX_RECORDS {
        return false;
    }
    let mut referenced = BTreeSet::new();
    for attachment in attachments {
        match attachment.get("type").and_then(Value::as_str) {
            Some("file") => {
                let Some(path) = attachment.get("path").and_then(Value::as_str) else {
                    return false;
                };
                if !owned_path(project, path) {
                    return false;
                }
                referenced.insert(path);
            }
            Some("inline") => {}
            Some("external") => continue,
            _ => return false,
        }
        match attachment.get("original_path") {
            None | Some(Value::Null) => {}
            Some(Value::String(path)) if owned_path(project, path) => {
                referenced.insert(path.as_str());
            }
            Some(_) => return false,
        }
    }
    extra
        .iter()
        .all(|path| owned_path(project, path) && referenced.contains(path.as_str()))
}

#[cfg(test)]
mod tests {
    use super::super::super as database;
    use super::super::{
        Config, Cx, DbPool, PreparedMessage, ReconcileCursor, WriteOp, clear_by_operator,
        ledger, outcome, prepare_message_classified, record_failures,
    };
    use super::*;
    use fastmcp_core::block_on;
    use serde_json::json;
    use sha1::{Digest as _, Sha1};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const RAW: &str = "projects/project/attachments/files/data.bin";
    const ORIGINAL: &str = "projects/project/attachments/originals/data.bin";
    const BYTES: &[u8] = b"retained attachment\x00\xff\n";

    fn metadata() -> Value {
        json!([{
            "type": "file", "path": RAW, "original_path": ORIGINAL,
            "bytes": BYTES.len(), "sha1": hex::encode(Sha1::digest(BYTES)),
        }])
    }

    #[test]
    fn referenced_files_and_retained_originals_cover_extra_obligations() {
        let message = json!({"attachments": metadata()});
        for paths in [
            vec![],
            vec![RAW.into()],
            vec![ORIGINAL.into()],
            vec![RAW.into(), ORIGINAL.into(), RAW.into()],
        ] {
            assert!(covered_by_message("project", &message, &paths));
        }
        let inline = json!({"attachments": [{"type": "inline", "original_path": ORIGINAL}]});
        assert!(covered_by_message("project", &inline, &[ORIGINAL.into()]));
        assert!(!covered_by_message("project", &inline, &[RAW.into()]));
    }

    #[test]
    fn nearby_files_external_metadata_and_unknown_types_do_not_supply_authority() {
        let message = json!({"attachments": metadata()});
        for path in [
            "projects/project/attachments/manifests/2026-10.json",
            "projects/project/attachments/audit.log",
            "projects/project/agents/BlueLake/profile.json",
            "projects/other/attachments/files/data.bin",
        ] {
            assert!(!covered_by_message("project", &message, &[path.into()]), "{path}");
        }
        for kind in ["external", "unknown"] {
            let message = json!({"attachments": [{"type": kind, "path": RAW, "original_path": ORIGINAL}]});
            assert!(!covered_by_message(
                "project", &message, &[RAW.into(), ORIGINAL.into()]
            ));
        }
    }

    #[test]
    fn even_matching_metadata_cannot_authorize_noncanonical_or_foreign_paths() {
        for path in [
            "projects/project/attachments/../agents/BlueLake/profile.json",
            "projects/project/attachments//data.bin",
            "projects/project/attachments/./data.bin",
            "projects/project/attachments/data.bin/",
            "projects/project/attachments/",
            "projects/project/attachments/a:b",
            "projects/project/attachments/a\\b",
            "projects/project/attachments/a\nb",
            "/projects/project/attachments/files/data.bin",
            "projects/project-two/attachments/files/data.bin",
        ] {
            let message = json!({"attachments": [{"type": "file", "path": path}]});
            assert!(!covered_by_message("project", &message, &[path.into()]), "{path:?}");
        }
        let path = format!("projects/project/attachments/{}", "x".repeat(MAX_PATH_BYTES));
        let message = json!({"attachments": [{"type": "file", "path": path}]});
        assert!(!covered_by_message("project", &message, &[path]));
    }

    #[test]
    fn malformed_metadata_and_collection_overflow_remain_unknown_obligations() {
        for attachments in [
            Value::Null,
            json!({}),
            json!([null]),
            json!([{"type":"file"}]),
            json!([{"type":"file","path":7}]),
            json!([{"type":"inline","original_path":false}]),
        ] {
            assert!(!covered_by_message(
                "project", &json!({"attachments": attachments}), &[RAW.into()]
            ));
        }
        let record = json!({"type":"file", "path":RAW, "original_path":ORIGINAL});
        let message = json!({"attachments": vec![record.clone(); MAX_RECORDS]});
        assert!(covered_by_message("project", &message, &vec![RAW.into(); MAX_PATHS]));
        assert!(!covered_by_message("project", &message, &vec![RAW.into(); MAX_PATHS + 1]));
        let message = json!({"attachments": vec![record; MAX_RECORDS + 1]});
        assert!(!covered_by_message("project", &message, &[RAW.into()]));
    }

    fn seed(cx: &Cx, pool: &DbPool) -> PreparedMessage {
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(&format!(
            "UPDATE messages SET attachments = '{}' WHERE id = 901", metadata()
        ))
        .unwrap();
        drop(conn);
        prepare_message_classified(cx, pool, 901).unwrap()
    }

    fn op(config: &Config, prepared: &PreparedMessage, extra_paths: Vec<String>) -> WriteOp {
        WriteOp::MessageBundle {
            config: config.clone(),
            project_slug: prepared.project_slug.clone(),
            message_json: prepared.message.clone(),
            body_md: prepared.body.clone(),
            sender: prepared.sender.clone(),
            recipients: prepared.recipients.clone(),
            extra_paths,
        }
    }

    fn write_attachments(config: &Config) {
        for relative in [RAW, ORIGINAL] {
            let path = config.storage_root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, BYTES).unwrap();
        }
    }

    fn drive(cx: &Cx, pool: &DbPool, config: &Config) -> database::ReconcileReport {
        database::reconcile_message_batch(
            cx,
            pool,
            config,
            &mut ReconcileCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn assert_archive(config: &Config, prepared: &PreparedMessage) {
        let archive = crate::ensure_archive(config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive, &prepared.message, &prepared.sender, &prepared.recipients,
        )
        .unwrap()
        .0;
        let committed = database::CommittedMessages::open(&archive).unwrap();
        for path in std::iter::once(&paths.canonical)
            .chain(std::iter::once(&paths.outbox))
            .chain(paths.inbox.iter())
        {
            let (message, body) = committed.read(&archive, path).unwrap().unwrap();
            assert_eq!(
                database::read_surviving_message(path).unwrap().unwrap(),
                (message.clone(), body.clone())
            );
            assert_eq!(body, prepared.body);
            let mut expected = prepared.message.clone();
            if paths.inbox.contains(path) {
                expected["bcc"] = json!([]);
            }
            assert_eq!(message, expected);
        }
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        for relative in [RAW, ORIGINAL] {
            let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
            assert_eq!(repo.find_blob(entry.id()).unwrap().content(), BYTES);
            assert_eq!(std::fs::read(config.storage_root.join(relative)).unwrap(), BYTES);
        }
    }

    fn assert_source(cx: &Cx, pool: &DbPool) {
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 901", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<String>("attachments").unwrap(), metadata().to_string());
        let rows = conn
            .query_sync("SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.get_named::<i64>("read_ts").unwrap(), 2_000_000);
            assert_eq!(row.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
        }
    }

    #[test]
    fn real_wbq_attachment_failure_recovers_and_clears_only_after_committed_repair() {
        database::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = seed(cx, pool);
            write_attachments(config);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive, &prepared.message, &prepared.sender, &prepared.recipients,
            )
            .unwrap()
            .0;
            std::fs::create_dir_all(&paths.canonical).unwrap();
            let before = crate::wbq_stats();
            // Real queue, drain, retry exhaustion and failure registration;
            // only the queue's ownership is private to this isolated fixture.
            let (sender, receiver) = std::sync::mpsc::sync_channel(2);
            let depth = Arc::new(AtomicU64::new(0));
            let envelope = crate::WbqMsg::Op(crate::WbqOpEnvelope {
                enqueued_at: Instant::now(),
                op: Box::new(op(config, &prepared, vec![RAW.into(), ORIGINAL.into()])),
            });
            assert!(crate::wbq_try_send_counted(&sender, &depth, envelope, None).is_ok());
            assert!(sender.send(crate::WbqMsg::Shutdown).is_ok());
            drop(sender);
            crate::wbq_drain_loop(Arc::new(Mutex::new(Some(receiver))), depth.clone(), 2, 2);
            crate::flush_async_commits();
            assert_eq!(depth.load(Ordering::Relaxed), 0);
            let failed = crate::wbq_stats();
            assert_eq!(failed.unrecoverable_errors, before.unrecoverable_errors + 1);
            assert!(crate::durability_degraded());
            assert_eq!(ledger().entries.len(), 1);
            assert!(!ledger().unknown_failure, "referenced attachments have repair authority");
            std::fs::rename(
                &paths.canonical,
                config.storage_root.join("held-attachment-obstruction"),
            )
            .unwrap();
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert!(!crate::durability_degraded());
            assert!(ledger().entries.is_empty());
            assert_archive(config, &prepared);
            assert_source(cx, pool);
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let head = repo.head().unwrap().target();
            assert_eq!(drive(cx, pool, config).repaired, 0);
            assert_eq!(repo.head().unwrap().target(), head);
            assert_eq!(crate::wbq_stats().unrecoverable_errors, failed.unrecoverable_errors);
        });
    }

    fn unavailable_attachment(conflicting: bool) {
        database::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = seed(cx, pool);
            if conflicting {
                write_attachments(config);
                std::fs::write(config.storage_root.join(RAW), b"conflicting bytes").unwrap();
            }
            record_failures(std::iter::once(&op(
                config, &prepared, vec![RAW.into(), ORIGINAL.into()],
            )));
            assert_eq!(ledger().entries.len(), 1);
            assert!(!ledger().unknown_failure);
            let report = drive(cx, pool, config);
            assert_eq!(report.repaired, 0);
            assert!(report.deferred > 0);
            assert!(crate::durability_degraded());
            assert_eq!(ledger().entries.len(), 1);
            if conflicting {
                assert_eq!(
                    std::fs::read(config.storage_root.join(RAW)).unwrap(),
                    b"conflicting bytes"
                );
            } else {
                assert!(!config.storage_root.join(RAW).exists());
            }
            assert_source(cx, pool);
            // Restoring the original content allows a later ordinary pass to
            // verify it. Neither the failure nor restoration bypasses Git proof.
            write_attachments(config);
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert!(!crate::durability_degraded());
            assert_archive(config, &prepared);
            assert_source(cx, pool);
        });
    }

    #[test]
    fn missing_attachment_bytes_keep_the_latch_until_the_bytes_are_restored() {
        unavailable_attachment(false);
    }

    #[test]
    fn conflicting_attachment_bytes_are_preserved_and_cannot_clear_the_latch() {
        unavailable_attachment(true);
    }

    #[test]
    fn unreferenced_extra_file_keeps_manual_recovery_even_after_message_converges() {
        database::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = seed(cx, pool);
            write_attachments(config);
            let unknown = "projects/project/attachments/manifests/operator-evidence.json";
            let path = config.storage_root.join(unknown);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"unverified obligation").unwrap();
            record_failures(std::iter::once(&op(
                config, &prepared, vec![RAW.into(), unknown.into()],
            )));
            assert!(ledger().unknown_failure);
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert!(crate::durability_degraded());
            assert_eq!(std::fs::read(&path).unwrap(), b"unverified obligation");
            assert_archive(config, &prepared);
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            assert!(tree.get_path(std::path::Path::new(unknown)).is_err());
            assert_source(cx, pool);
            clear_by_operator();
        });
    }
}
