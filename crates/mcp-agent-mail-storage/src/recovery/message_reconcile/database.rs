//! Bounded live-DB to archive convergence for the maintenance worker.
//!
//! This is not the archive-to-DB reconstruction path. It never substitutes an
//! archive snapshot for the live source and never modifies mailbox rows. The
//! worker must supply its live pool for the same configured mailbox/root.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbError, DbPool, corruption_circuit_breaker};
use serde::Serialize;
use serde_json::{Value, json};

use super::{ReconcileResult, read_surviving_message, reconcile_message_bundle};
use crate::{MessageBundleBatchEntry, ProjectArchive};

const IDS_PER_LANE: i64 = 16;
const MAX_REPAIRS_PER_BATCH: usize = 4;
const MAX_DB_PAYLOAD_BYTES: i64 = 4 * 1024 * 1024;
const MAX_BATCH_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const NORMAL_ARCHIVE_GRACE_US: i64 = 30 * 1_000_000;

/// Separate new-message and rotating backfill cursors prevent old broken
/// records from pinning catch-up and continuous new mail from starving history.
/// Cursors are only hints: restarting or wrapping always safely checks again.
#[derive(Debug, Default)]
pub struct ReconcileCursor {
    source_identity: String,
    tail_after: Option<i64>,
    backfill_ceiling: Option<i64>,
}

impl ReconcileCursor {
    fn advance(&mut self, id: i64, tail: bool) {
        if tail {
            self.tail_after = Some(self.tail_after.unwrap_or(0).max(id));
        } else {
            self.backfill_ceiling = Some(id.saturating_sub(1));
        }
    }
}

/// Counts describe this bounded pass, not whole-mailbox durability.
#[derive(Debug, Default, Serialize)]
pub struct ReconcileReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub repaired: usize,
    pub files_created: usize,
    pub deferred: usize,
    pub payload_bytes: usize,
    pub interrupted: bool,
    pub budget_exhausted: bool,
}

/// Default-on non-destructive message repair, independently switchable from
/// opt-in destructive message retention. Invalid overrides fail disabled.
#[must_use]
pub fn enabled() -> bool {
    parse_enabled(
        mcp_agent_mail_core::config::process_env_value("AM_MESSAGE_ARCHIVE_RECONCILE_ENABLED")
            .as_deref(),
    )
}

fn parse_enabled(raw: Option<&str>) -> bool {
    raw.is_none_or(|raw| {
        matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("message reconciliation source read cancelled".to_string()),
        Outcome::Panicked(_) => Err("message reconciliation source read panicked".to_string()),
    }
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("message reconciliation source query failed: {error}")
}

fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    cursor: &mut ReconcileCursor,
    cutoff: i64,
) -> Result<Vec<(i64, bool)>, String> {
    let identity = pool.sqlite_identity_key();
    if identity != cursor.source_identity {
        *cursor = ReconcileCursor { source_identity: identity, ..Default::default() };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn.query_sync("SELECT COALESCE(MAX(id), 0) AS max_id FROM messages", &[])
        .map_err(source_error)?;
    let max_id = rows.first().ok_or_else(|| "message ID aggregate returned no row".to_string())?
        .get_named::<i64>("max_id").map_err(source_error)?;
    if cursor.tail_after.is_some_and(|after| after > max_id) {
        cursor.tail_after = None;
        cursor.backfill_ceiling = None;
    }
    let mut tail = if let Some(after) = cursor.tail_after {
        conn.query_sync(
            "SELECT id FROM messages WHERE id > ? AND created_ts <= ? ORDER BY id ASC LIMIT ?",
            &[after.into(), cutoff.into(), IDS_PER_LANE.into()],
        )
    } else {
        conn.query_sync(
            "SELECT id FROM messages WHERE created_ts <= ? ORDER BY id DESC LIMIT ?",
            &[cutoff.into(), IDS_PER_LANE.into()],
        )
    }.map_err(source_error)?.into_iter().map(|row| {
        row.get_named::<i64>("id").map_err(source_error)
    }).collect::<Result<Vec<_>, _>>()?;
    // Advance the tail in ascending ID order even on its initial recent slice.
    tail.sort_unstable();
    let history = conn.query_sync(
        "SELECT id FROM messages WHERE id > 0 AND id <= ? AND created_ts <= ? ORDER BY id DESC LIMIT ?",
        &[cursor.backfill_ceiling.unwrap_or(max_id).into(), cutoff.into(), IDS_PER_LANE.into()],
    ).map_err(source_error)?.into_iter().map(|row| {
        row.get_named::<i64>("id").map_err(source_error)
    }).collect::<Result<Vec<_>, _>>()?;
    if history.is_empty() {
        cursor.backfill_ceiling = None;
    }
    // Interleave the lanes so a per-pass repair/byte budget cannot starve one.
    let mut selected = Vec::with_capacity(tail.len() + history.len());
    for index in 0..tail.len().max(history.len()) {
        if let Some(id) = tail.get(index) {
            selected.push((*id, true));
        }
        if let Some(id) = history.get(index) {
            selected.push((*id, false));
        }
    }
    Ok(selected)
}

struct PreparedMessage {
    message: Value,
    body: String,
    sender: String,
    project_slug: String,
    recipients: Vec<String>,
    payload_bytes: usize,
}

/// One joined SELECT binds message, project and sender to the same observation.
/// The size predicate runs before the application materializes any payload.
fn prepare_message(cx: &Cx, pool: &DbPool, id: i64) -> Result<PreparedMessage, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn.query_sync(
        "SELECT m.id, m.subject, m.body_md, m.thread_id, m.topic, m.importance, \
         m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
         p.slug AS project_slug, p.human_key AS project_key, a.name AS sender \
         FROM messages m JOIN projects p ON p.id = m.project_id \
         JOIN agents a ON a.id = m.sender_id AND a.project_id = m.project_id \
         WHERE m.id = ? AND \
         length(CAST(m.body_md AS BLOB)) + length(CAST(m.subject AS BLOB)) + \
         length(CAST(m.recipients_json AS BLOB)) + length(CAST(m.attachments AS BLOB)) + \
         length(CAST(m.importance AS BLOB)) + length(CAST(p.slug AS BLOB)) + \
         length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
         COALESCE(length(CAST(m.thread_id AS BLOB)), 0) + \
         COALESCE(length(CAST(m.topic AS BLOB)), 0) <= ?",
        &[id.into(), MAX_DB_PAYLOAD_BYTES.into()],
    ).map_err(source_error)?;
    if rows.len() != 1 {
        return Err("message missing, oversized, or lacking an unambiguous project/sender".to_string());
    }
    let row = &rows[0];
    let text = |name| row.get_named::<String>(name).map_err(source_error);
    let body = text("body_md")?;
    let sender = text("sender")?;
    let project_slug = text("project_slug")?;
    let created = row.get_named::<i64>("created_ts").map_err(source_error)?;
    if chrono::DateTime::from_timestamp_micros(created).is_none() {
        return Err("message creation timestamp cannot be represented faithfully".to_string());
    }
    let ack = row.get_named::<i64>("ack_required").map_err(source_error)?;
    if !matches!(ack, 0 | 1) {
        return Err("message acknowledgment flag is not boolean".to_string());
    }
    let routing: Value = serde_json::from_str(&text("recipients_json")?)
        .map_err(|_| "message recipient metadata is not valid JSON".to_string())?;
    let attachments: Value = serde_json::from_str(&text("attachments")?)
        .map_err(|_| "message attachment metadata is not valid JSON".to_string())?;
    if !attachments.is_array() {
        return Err("message attachment metadata is not an array".to_string());
    }
    let mut recipients = Vec::new();
    for kind in ["to", "cc", "bcc"] {
        for name in routing.get(kind).and_then(Value::as_array)
            .ok_or_else(|| format!("message recipient metadata lacks {kind} array"))?
        {
            let name = name.as_str().ok_or_else(|| "non-string message recipient".to_string())?;
            crate::validate_archive_component("recipient", name).map_err(|error| error.to_string())?;
            recipients.push(name.to_string());
            if recipients.len() > super::MAX_RECIPIENTS {
                return Err("message recipient budget exceeded".to_string());
            }
        }
    }
    if recipients.is_empty() {
        return Err("message has no authoritative recipients".to_string());
    }
    recipients.sort_unstable();
    recipients.dedup();
    let message = json!({
        "id": id,
        "from": sender,
        "to": routing["to"],
        "cc": routing["cc"],
        "bcc": routing["bcc"],
        "subject": text("subject")?,
        "created": mcp_agent_mail_db::micros_to_iso(created),
        "thread_id": row.get_named::<Option<String>>("thread_id").map_err(source_error)?,
        "topic": row.get_named::<Option<String>>("topic").map_err(source_error)?,
        "project": text("project_key")?,
        "project_slug": project_slug,
        "importance": text("importance")?,
        "ack_required": ack != 0,
        "attachments": attachments,
    });
    let payload_bytes = body.len().saturating_add(message.to_string().len());
    Ok(PreparedMessage { message, body, sender, project_slug, recipients, payload_bytes })
}

fn validate_surviving_message(
    expected: &PreparedMessage,
    observed: &Value,
    body: &str,
) -> Result<(), String> {
    if body != expected.body {
        return Err("surviving archive body conflicts with the live message; preserved".to_string());
    }
    for key in ["id", "from", "subject", "project", "project_slug", "importance", "ack_required", "attachments"] {
        if observed.get(key) != expected.message.get(key) {
            return Err(format!("surviving archive {key} conflicts with live message; preserved"));
        }
    }
    for key in ["topic", "thread_id"] {
        if observed.get(key).unwrap_or(&Value::Null) != &expected.message[key] {
            return Err(format!("surviving archive {key} conflicts with live message; preserved"));
        }
    }
    let timestamp = |value: &Value| {
        value.get("created").and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_micros())
    };
    if timestamp(observed).is_none() || timestamp(observed) != timestamp(&expected.message) {
        return Err("surviving archive creation timestamp conflicts with live message; preserved".to_string());
    }
    for kind in ["to", "cc", "bcc"] {
        let names = |value: &Value| -> Option<Vec<String>> {
            let mut names = value.get(kind)?.as_array()?.iter()
                .map(|name| name.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
            names.sort_unstable();
            Some(names)
        };
        if names(observed).is_none() || names(observed) != names(&expected.message) {
            return Err(format!("surviving archive {kind} routing conflicts with live message; preserved"));
        }
    }
    if let Some(parent) = observed.get("reply_to")
        && parent.as_i64().is_none_or(|parent| parent <= 0 || Some(parent) == observed["id"].as_i64())
    {
        return Err("surviving archive reply parent is invalid; preserved".to_string());
    }
    Ok(())
}

fn reconcile_prepared(
    config: &Config,
    prepared: &PreparedMessage,
) -> Result<ReconcileResult, String> {
    let archive = crate::ensure_archive(config, &prepared.project_slug).map_err(|error| error.to_string())?;
    let paths = crate::message_paths_for_bundle(
        &archive, &prepared.message, &prepared.sender, &prepared.recipients,
    ).map_err(|error| error.to_string())?.0;
    let mut surviving = None;
    for path in [&paths.canonical, &paths.outbox] {
        if let Some((message, body)) = read_surviving_message(path).map_err(|error| error.to_string())? {
            validate_surviving_message(prepared, &message, &body)?;
            if let Some(previous) = &surviving {
                if previous != &message {
                    return Err("canonical and outbox metadata disagree; both preserved".to_string());
                }
            } else {
                surviving = Some(message);
            }
        }
    }
    // Disk loss can leave the exact canonical payload recoverable from Git.
    if surviving.is_none() {
        for path in [&paths.canonical, &paths.outbox] {
            if let Some((message, body)) = read_committed_message(&archive, path)? {
                validate_surviving_message(prepared, &message, &body)?;
                surviving = Some(message);
                break;
            }
        }
    }
    let message = match surviving {
        Some(message) => message,
        None if prepared.message["thread_id"].is_null() => prepared.message.clone(),
        None => {
            // SQLite stores the thread, not the immediate reply parent. A
            // fabricated payload can collide with a delayed original WBQ write.
            return Err("threaded message has no surviving authoritative bundle; reply metadata cannot be inferred".to_string());
        }
    };
    reconcile_message_bundle(&archive, config, MessageBundleBatchEntry {
        message: &message,
        body_md: &prepared.body,
        sender: &prepared.sender,
        recipients: &prepared.recipients,
        extra_paths: &[],
    }).map_err(|error| error.to_string())
}

fn read_committed_message(
    archive: &ProjectArchive,
    path: &std::path::Path,
) -> Result<Option<(Value, String)>, String> {
    let repo = git2::Repository::open(
        crate::archive_repo_root_checked(archive).map_err(|error| error.to_string())?,
    ).map_err(|error| error.to_string())?;
    let head = match repo.head() {
        Ok(head) => head,
        Err(error) if matches!(error.code(), git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).map_err(|error| error.to_string())?;
    let tree = head.peel_to_tree().map_err(|error| error.to_string())?;
    let entry = match tree.get_path(std::path::Path::new(&relative)) {
        Ok(entry) => entry,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if entry.kind() != Some(git2::ObjectType::Blob) || !matches!(entry.filemode(), 0o100644 | 0o100755) {
        return Err("committed message is not a regular-file blob".to_string());
    }
    let odb = repo.odb().map_err(|error| error.to_string())?;
    let (size, kind) = odb.read_header(entry.id()).map_err(|error| error.to_string())?;
    if kind != git2::ObjectType::Blob || size > super::MAX_MESSAGE_ARTIFACT_BYTES {
        return Err("committed message exceeds the archive recovery byte bound".to_string());
    }
    let blob = repo.find_blob(entry.id()).map_err(|error| error.to_string())?;
    let text = std::str::from_utf8(blob.content()).map_err(|_| "committed message is not UTF-8".to_string())?;
    let (frontmatter, body) = text.strip_prefix("---json\n")
        .and_then(|text| text.split_once("\n---\n\n"))
        .ok_or_else(|| "committed message has invalid canonical frontmatter".to_string())?;
    let message = serde_json::from_str(frontmatter).map_err(|_| "committed message has invalid JSON".to_string())?;
    Ok(Some((message, body.to_string())))
}

/// Reconcile a bounded pass against the server's live mailbox pool.
///
/// Recent catch-up and rotating history each select at most 16 IDs. At most
/// four repairs and 16 MiB of projected payload are handled per pass. These
/// are work/memory bounds, not a hard deadline on filesystem or libgit2 calls.
/// Normal archive writes receive a 30-second grace window. No row, receipt,
/// delivery, notification or thread digest is mutated. A failed message is
/// reported and revisited by backfill, not retried in a tight loop.
///
/// # Errors
///
/// Refuses source acquisition/query errors or an open corruption breaker.
/// Individual ambiguous/conflicting/oversized artifacts count as deferred.
pub fn reconcile_message_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReconcileReport, String> {
    let mut report = ReconcileReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if corruption_circuit_breaker().is_tripped() {
        return Err("message reconciliation refused: source corruption breaker is open".to_string());
    }
    // A published query-only snapshot must never become repair authority for
    // another live mailbox. Bind the caller's root to the pool's frozen root.
    let pool_root = std::fs::canonicalize(pool.storage_root()).map_err(|error| error.to_string())?;
    let configured_root = std::fs::canonicalize(&config.storage_root).map_err(|error| error.to_string())?;
    if pool_root != configured_root {
        return Err("message reconciliation pool and archive roots do not match".to_string());
    }
    let cutoff = mcp_agent_mail_db::now_micros().saturating_sub(NORMAL_ARCHIVE_GRACE_US);
    let selected = select_ids(cx, pool, cursor, cutoff)?;
    let mut seen = HashSet::new();
    for (id, tail) in selected {
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("message reconciliation stopped: source corruption observed".to_string());
        }
        if report.repaired >= MAX_REPAIRS_PER_BATCH {
            report.budget_exhausted = true;
            break;
        }
        if !seen.insert(id) {
            cursor.advance(id, tail);
            continue;
        }
        // Freeze recovery promotion through source observation and archive
        // publication. Drop each SQL connection before any filesystem/Git work.
        let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
        let result = match prepare_message(cx, pool, id) {
            Ok(prepared) => {
                if prepared.payload_bytes > MAX_BATCH_PAYLOAD_BYTES.saturating_sub(report.payload_bytes) {
                    report.budget_exhausted = true;
                    break;
                }
                report.payload_bytes += prepared.payload_bytes;
                if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                    report.interrupted = true;
                    break;
                }
                reconcile_prepared(config, &prepared)
            }
            Err(error) => Err(error),
        };
        report.scanned += 1;
        match result {
            Ok(result) if result.files_created > 0 || result.git_commit_needed => {
                report.repaired += 1;
                report.files_created += result.files_created;
            }
            Ok(_) => report.unchanged += 1,
            Err(error) => {
                report.deferred += 1;
                tracing::warn!(
                    target: "maintenance", event = "message_archive_reconcile_deferred",
                    message_id = id, reason = %error,
                    "message archive repair deferred; source and conflicting evidence retained"
                );
            }
        }
        cursor.advance(id, tail);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> PreparedMessage {
        let message = json!({
            "id": 9, "from": "BlueLake", "to": ["GreenStone"], "cc": [], "bcc": ["RedFox"],
            "subject": "handoff", "created": "2026-09-17T01:02:03.123456Z",
            "thread_id": "thread", "topic": null, "project": "/project", "project_slug": "project",
            "importance": "normal", "ack_required": false, "attachments": [],
        });
        PreparedMessage { message, body: "body\n".into(), sender: "BlueLake".into(), project_slug: "project".into(),
            recipients: vec!["GreenStone".into(), "RedFox".into()], payload_bytes: 512 }
    }

    #[test]
    fn enable_override_is_explicit_and_invalid_values_fail_disabled() {
        assert!(parse_enabled(None));
        for raw in ["true", " 1 ", "YES", "on"] { assert!(parse_enabled(Some(raw))); }
        for raw in ["false", "0", "off", "no", "", "typo"] { assert!(!parse_enabled(Some(raw))); }
    }

    #[test]
    fn surviving_reply_metadata_is_preserved_but_conflicting_fields_refuse() {
        let original = prepared();
        let mut message = original.message.clone();
        message["reply_to"] = json!(7);
        message["future_metadata"] = json!({"opaque": true});
        validate_surviving_message(&original, &message, &original.body).unwrap();
        for (key, value) in [("id", json!(10)), ("from", json!("RedFox")), ("to", json!(["RedFox"])),
            ("bcc", json!([])), ("project", json!("/other")), ("reply_to", json!(9)), ("created", json!("invalid"))] {
            let mut changed = message.clone();
            changed[key] = value;
            assert!(validate_surviving_message(&original, &changed, &original.body).is_err(), "{key}");
        }
        assert!(validate_surviving_message(&original, &message, "different body").is_err());
        assert_eq!(message["reply_to"], 7);
    }

    #[test]
    fn unarchived_thread_is_deferred_instead_of_inventing_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config { storage_root: temp.path().to_path_buf(), ..Config::default() };
        let original = prepared();
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(error.contains("reply metadata cannot be inferred"));
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(&archive, &original.message, &original.sender, &original.recipients).unwrap().0;
        assert!(!paths.canonical.exists());
    }

    #[test]
    fn real_file_backed_projection_repairs_without_modifying_mailbox_rows() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(), min_connections: 1, max_connections: 1,
                ..Default::default()
            }).unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
                VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                VALUES(901, 101, 101, 'handoff', 'body', 'normal', 0, 1000000, '{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[]}', '[]')").unwrap();
            conn.execute_raw("INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 102, 'to')").unwrap();
            let before = conn.query_sync("SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901", &[]).unwrap();
            assert_eq!(before[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(before[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            drop(conn);
            let config = Config { storage_root: pool.storage_root().to_path_buf(), database_url, ..Config::default() };
            let mut cursor = ReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let report = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
            assert_eq!(report.scanned, 1);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 3);
            assert_eq!(report.deferred, 0);
            let again = reconcile_message_batch(&cx, &pool, &config, &mut ReconcileCursor::default(), &stop).unwrap();
            assert_eq!(again.repaired, 0);
            assert_eq!(again.unchanged, 1);
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            let after = conn.query_sync("SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901", &[]).unwrap();
            assert_eq!(after[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(after[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            let rows = conn.query_sync("SELECT body_md FROM messages WHERE id = 901", &[]).unwrap();
            assert_eq!(rows[0].get_named::<String>("body_md").unwrap(), "body");
        });
    }
}
