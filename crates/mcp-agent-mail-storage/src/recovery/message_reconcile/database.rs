//! Bounded live-DB to archive convergence for the maintenance worker.
//!
//! This is not the archive-to-DB reconstruction path. It never substitutes an
//! archive snapshot for the live source and never modifies mailbox rows. The
//! worker must supply its live pool for the same configured mailbox/root.

mod source;
mod staged;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbError, DbPool, corruption_circuit_breaker};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use self::source::prepare_message;
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
    // A byte budget can admit only one message. Resume with the opposite lane
    // after the last consumed item, not unconditionally with new-mail catch-up.
    next_lane_is_history: bool,
}

impl ReconcileCursor {
    fn advance(&mut self, id: i64, tail: bool) {
        if tail {
            self.tail_after = Some(self.tail_after.unwrap_or(0).max(id));
        } else {
            self.backfill_ceiling = Some(id.saturating_sub(1));
        }
        self.next_lane_is_history = tail;
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
    /// Serialized payload bytes admitted to archive work, excluding rejected
    /// oversized projections (whose SQL input has a separate byte bound).
    pub payload_bytes: usize,
    pub interrupted: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadAdmission {
    Fits,
    NextBatch,
    Oversized,
}

fn payload_admission(used: usize, candidate: usize) -> PayloadAdmission {
    if candidate > MAX_BATCH_PAYLOAD_BYTES {
        PayloadAdmission::Oversized
    } else if candidate > MAX_BATCH_PAYLOAD_BYTES.saturating_sub(used) {
        PayloadAdmission::NextBatch
    } else {
        PayloadAdmission::Fits
    }
}

/// Default-on non-destructive repair for file-backed mailboxes, independently
/// switchable from destructive retention. Invalid overrides fail disabled.
#[must_use]
pub fn enabled(config: &Config) -> bool {
    mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url).is_some()
        && parse_enabled(
            mcp_agent_mail_core::config::process_env_value("AM_MESSAGE_ARCHIVE_RECONCILE_ENABLED")
                .as_deref(),
        )
}

fn parse_enabled(raw: Option<&str>) -> bool {
    raw.is_none_or(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
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

fn validate_pool_binding(pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or_else(|| "message reconciliation requires a file-backed source".to_string())?;
    let selected = std::fs::canonicalize(selected).map_err(|error| error.to_string())?;
    let source = std::fs::canonicalize(pool.sqlite_path()).map_err(|error| error.to_string())?;
    if source != selected {
        return Err("message reconciliation pool is not the configured live database".to_string());
    }
    let pool_root =
        std::fs::canonicalize(pool.storage_root()).map_err(|error| error.to_string())?;
    let configured_root =
        std::fs::canonicalize(&config.storage_root).map_err(|error| error.to_string())?;
    if pool_root != configured_root {
        return Err("message reconciliation pool and archive roots do not match".to_string());
    }
    Ok(())
}

fn interleave_ids(tail: &[i64], history: &[i64], history_first: bool) -> Vec<(i64, bool)> {
    let mut selected = Vec::with_capacity(tail.len() + history.len());
    let lanes = if history_first {
        [(history, false), (tail, true)]
    } else {
        [(tail, true), (history, false)]
    };
    for index in 0..tail.len().max(history.len()) {
        for (ids, is_tail) in lanes {
            if let Some(id) = ids.get(index) {
                // Keep duplicate IDs: the caller processes the payload once but
                // advances both independent cursors when it consumes each lane.
                selected.push((*id, is_tail));
            }
        }
    }
    selected
}

fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    cursor: &mut ReconcileCursor,
    cutoff: i64,
) -> Result<Vec<(i64, bool)>, String> {
    let identity = pool.sqlite_identity_key();
    if identity != cursor.source_identity {
        *cursor = ReconcileCursor {
            source_identity: identity,
            ..Default::default()
        };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let mode = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    let query_only = mode
        .first()
        .ok_or_else(|| "source query-only mode was not reported".to_string())?
        .get_as::<i64>(0)
        .map_err(source_error)?;
    if query_only != 0 {
        return Err("query-only snapshots cannot authorize message archive repair".to_string());
    }
    let rows = conn
        .query_sync("SELECT COALESCE(MAX(id), 0) AS max_id FROM messages", &[])
        .map_err(source_error)?;
    let max_id = rows
        .first()
        .ok_or_else(|| "message ID aggregate returned no row".to_string())?
        .get_named::<i64>("max_id")
        .map_err(source_error)?;
    if cursor.tail_after.is_some_and(|after| after > max_id) {
        cursor.tail_after = None;
        cursor.backfill_ceiling = None;
        cursor.next_lane_is_history = false;
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
    }
    .map_err(source_error)?
    .into_iter()
    .map(|row| row.get_named::<i64>("id").map_err(source_error))
    .collect::<Result<Vec<_>, _>>()?;
    tail.sort_unstable();
    let history = conn
        .query_sync(
            "SELECT id FROM messages WHERE id > 0 AND id <= ? AND created_ts <= ? ORDER BY id DESC LIMIT ?",
            &[
                cursor.backfill_ceiling.unwrap_or(max_id).into(),
                cutoff.into(),
                IDS_PER_LANE.into(),
            ],
        )
        .map_err(source_error)?
        .into_iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect::<Result<Vec<_>, _>>()?;
    if history.is_empty() {
        cursor.backfill_ceiling = None;
    }
    // Interleave within a pass AND retain lane priority across budget-limited
    // passes. Fixed tail-first ordering starves history at one payload per pass.
    Ok(interleave_ids(&tail, &history, cursor.next_lane_is_history))
}

struct PreparedMessage {
    message: Value,
    body: String,
    sender: String,
    project_slug: String,
    recipients: Vec<String>,
    payload_bytes: usize,
    // NULL legacy metadata cannot distinguish a reply from a threaded send.
    // Fresh writes persist either the exact parent or authoritative absence.
    archive_metadata_known: bool,
}

fn validate_surviving_message(
    expected: &PreparedMessage,
    observed: &Value,
    body: &str,
) -> Result<(), String> {
    if body != expected.body {
        return Err(
            "surviving archive body conflicts with the live message; preserved".to_string(),
        );
    }
    for key in [
        "id",
        "from",
        "subject",
        "project",
        "project_slug",
        "importance",
        "ack_required",
        "attachments",
    ] {
        if observed.get(key) != expected.message.get(key) {
            return Err(format!(
                "surviving archive {key} conflicts with live message; preserved"
            ));
        }
    }
    for key in ["topic", "thread_id"] {
        if observed.get(key).unwrap_or(&Value::Null) != &expected.message[key] {
            return Err(format!(
                "surviving archive {key} conflicts with live message; preserved"
            ));
        }
    }
    let timestamp = |value: &Value| {
        value
            .get("created")
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_micros())
    };
    if timestamp(observed).is_none() || timestamp(observed) != timestamp(&expected.message) {
        return Err(
            "surviving archive creation timestamp conflicts with live message; preserved"
                .to_string(),
        );
    }
    for kind in ["to", "cc", "bcc"] {
        let names = |value: &Value| -> Option<Vec<String>> {
            let mut names = value
                .get(kind)?
                .as_array()?
                .iter()
                .map(|name| name.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            names.sort_unstable();
            Some(names)
        };
        if names(observed).is_none() || names(observed) != names(&expected.message) {
            return Err(format!(
                "surviving archive {kind} routing conflicts with live message; preserved"
            ));
        }
    }
    if let Some(parent) = observed.get("reply_to")
        && parent
            .as_i64()
            .is_none_or(|parent| parent <= 0 || Some(parent) == observed["id"].as_i64())
    {
        return Err("surviving archive reply parent is invalid; preserved".to_string());
    }
    if expected.archive_metadata_known
        && observed.get("reply_to") != expected.message.get("reply_to")
    {
        return Err(
            "surviving archive reply parent conflicts with durable message metadata; preserved"
                .to_string(),
        );
    }
    Ok(())
}

/// Inbox copies retain reply/extension metadata but deliberately redact BCC.
/// Restore only that redacted field from the live DB, then apply every normal
/// identity/body/routing check. Never trust an inbox to supply private routing.
fn restore_inbox_metadata(
    prepared: &PreparedMessage,
    mut message: Value,
    body: &str,
) -> Result<Value, String> {
    if !message
        .get("bcc")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return Err("surviving inbox has invalid BCC redaction; preserved".to_string());
    }
    message["bcc"] = prepared.message["bcc"].clone();
    validate_surviving_message(prepared, &message, body)?;
    Ok(message)
}

fn merge_surviving_metadata(
    surviving: &mut Option<Value>,
    message: Value,
    disagreement: &str,
) -> Result<(), String> {
    if let Some(previous) = surviving.as_ref() {
        if previous != &message {
            return Err(disagreement.to_string());
        }
    } else {
        *surviving = Some(message);
    }
    Ok(())
}

fn reconcile_prepared(
    config: &Config,
    prepared: &PreparedMessage,
) -> Result<ReconcileResult, String> {
    let archive =
        crate::ensure_archive(config, &prepared.project_slug).map_err(|error| error.to_string())?;
    let paths = crate::message_paths_for_bundle(
        &archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )
    .map_err(|error| error.to_string())?
    .0;
    let mut surviving = None;
    for path in [&paths.canonical, &paths.outbox] {
        if let Some((message, body)) =
            read_surviving_message(path).map_err(|error| error.to_string())?
        {
            validate_surviving_message(prepared, &message, &body)?;
            merge_surviving_metadata(
                &mut surviving,
                message,
                "canonical and outbox metadata disagree; both preserved",
            )?;
        }
    }
    // Open Git lazily, once. Every fallback candidate comes from one pinned
    // tree even if an archive writer advances HEAD while we inspect copies.
    let committed = if surviving.is_none() {
        Some(CommittedMessages::open(&archive)?)
    } else {
        None
    };
    if let Some(committed) = &committed {
        for path in [&paths.canonical, &paths.outbox] {
            if let Some((message, body)) = committed.read(&archive, path)? {
                validate_surviving_message(prepared, &message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "committed canonical and outbox metadata disagree; both preserved",
                )?;
            }
        }
    }
    // A crash can remove both full copies before Git commits them while
    // leaving an inbox intact. It still carries the exact reply parent and
    // extension fields; BCC comes exclusively from the authoritative DB row.
    if surviving.is_none() {
        for path in &paths.inbox {
            if let Some((message, body)) =
                read_surviving_message(path).map_err(|error| error.to_string())?
            {
                let message = restore_inbox_metadata(prepared, message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "surviving inbox metadata disagree; all copies preserved",
                )?;
            }
        }
    }
    // The working tree can lose every copy while Git still retains a redacted
    // inbox blob. It is just as useful as an on-disk inbox for recovering reply
    // metadata. Validate all committed inbox candidates, not only the first.
    if surviving.is_none()
        && let Some(committed) = &committed
    {
        for path in &paths.inbox {
            if let Some((message, body)) = committed.read(&archive, path)? {
                let message = restore_inbox_metadata(prepared, message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "committed inbox metadata disagree; all copies preserved",
                )?;
            }
        }
    }
    // Staging writes the exact blob before Git commits it. If every disk and
    // HEAD copy was lost, a bounded index snapshot can still retain reply and
    // extension metadata. It must agree with the live DB and with every other
    // staged copy; it never overrides unavailable/conflicting stronger evidence.
    if surviving.is_none()
        && let Some(committed) = &committed
    {
        surviving = staged::read_metadata(
            &committed.repo,
            &archive,
            prepared,
            &paths.canonical,
            &paths.outbox,
            &paths.inbox,
        )?;
    }
    let message = match surviving {
        Some(message) => message,
        None if prepared.archive_metadata_known || prepared.message["thread_id"].is_null() => {
            prepared.message.clone()
        }
        None => {
            // Legacy SQLite rows store only the thread. A fabricated parent
            // can collide with a delayed original WBQ write.
            return Err("threaded message has no surviving authoritative bundle; reply metadata cannot be inferred".to_string());
        }
    };
    reconcile_message_bundle(
        &archive,
        config,
        MessageBundleBatchEntry {
            message: &message,
            body_md: &prepared.body,
            sender: &prepared.sender,
            recipients: &prepared.recipients,
            extra_paths: &[],
        },
    )
    .map_err(|error| error.to_string())
}

/// Immutable Git observation for all surviving copies of one message.
/// Holding the tree identity avoids mixing metadata from separate HEADs.
struct CommittedMessages {
    repo: git2::Repository,
    tree_id: Option<git2::Oid>,
}

impl CommittedMessages {
    fn open(archive: &ProjectArchive) -> Result<Self, String> {
        let repo = git2::Repository::open(
            crate::archive_repo_root_checked(archive).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let tree_id = match repo.head() {
            Ok(head) => Some(head.peel_to_tree().map_err(|error| error.to_string())?.id()),
            Err(error)
                if matches!(
                    error.code(),
                    git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
                ) =>
            {
                None
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self { repo, tree_id })
    }

    fn read(
        &self,
        archive: &ProjectArchive,
        path: &std::path::Path,
    ) -> Result<Option<(Value, String)>, String> {
        let Some(tree_id) = self.tree_id else {
            return Ok(None);
        };
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path)
            .map_err(|error| error.to_string())?;
        let tree = self
            .repo
            .find_tree(tree_id)
            .map_err(|error| error.to_string())?;
        let entry = match tree.get_path(std::path::Path::new(&relative)) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if entry.kind() != Some(git2::ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
        {
            return Err("committed message is not a regular-file blob".to_string());
        }
        let odb = self.repo.odb().map_err(|error| error.to_string())?;
        let (size, kind) = odb
            .read_header(entry.id())
            .map_err(|error| error.to_string())?;
        if kind != git2::ObjectType::Blob || size > super::MAX_MESSAGE_ARTIFACT_BYTES {
            return Err("committed message exceeds the archive recovery byte bound".to_string());
        }
        let blob = self
            .repo
            .find_blob(entry.id())
            .map_err(|error| error.to_string())?;
        let actual_id = git2::Oid::hash_object_ext(
            git2::ObjectType::Blob,
            blob.content(),
            entry.id().object_format(),
        )
        .map_err(|error| error.to_string())?;
        if actual_id != entry.id() {
            return Err("committed message content does not match its object identity".to_string());
        }
        let text = std::str::from_utf8(blob.content())
            .map_err(|_| "committed message is not UTF-8".to_string())?;
        let (frontmatter, body) = text
            .strip_prefix("---json\n")
            .and_then(|text| text.split_once("\n---\n\n"))
            .ok_or_else(|| "committed message has invalid canonical frontmatter".to_string())?;
        let message = serde_json::from_str(frontmatter)
            .map_err(|_| "committed message has invalid JSON".to_string())?;
        Ok(Some((message, body.to_string())))
    }
}

/// Reconcile a bounded pass against the server's live mailbox pool.
///
/// Recent catch-up and rotating history each select at most 16 IDs. At most
/// four successful repairs and 16 MiB of serialized payload are admitted to
/// archive work per pass. SQL projections are separately bounded before JSON
/// serialization; an expanded payload that cannot fit any batch is deferred
/// without pinning the cursor. These are work bounds, not deadlines on SQL,
/// filesystem or libgit2 calls. Normal archive writes receive a 30-second grace.
/// No row, receipt, delivery, notification or thread digest is mutated. Failed
/// messages are reported and revisited by backfill, not retried in a tight loop.
///
/// # Errors
///
/// Refuses mismatched/readonly sources, source acquisition/query errors, or an
/// open corruption breaker. Ambiguous/conflicting/oversized artifacts defer.
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
        return Err(
            "message reconciliation refused: source corruption breaker is open".to_string(),
        );
    }
    validate_pool_binding(pool, config)?;
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
        // publication. Drop each SQL connection before filesystem/Git work.
        let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
        let result = match prepare_message(cx, pool, id) {
            Ok(prepared) => match payload_admission(report.payload_bytes, prepared.payload_bytes) {
                PayloadAdmission::Oversized => {
                    // SQL's raw-text bound does not bound JSON escaping. This
                    // item cannot fit an empty batch either: report it and
                    // advance below so all later mail can still converge.
                    Err(format!(
                        "serialized message payload exceeds per-batch limit ({} > {} bytes); source preserved",
                        prepared.payload_bytes, MAX_BATCH_PAYLOAD_BYTES,
                    ))
                }
                PayloadAdmission::NextBatch => {
                    // This item CAN fit a fresh batch. Do not consume its
                    // cursor or lane priority before it has been attempted.
                    report.budget_exhausted = true;
                    break;
                }
                PayloadAdmission::Fits => {
                    report.payload_bytes += prepared.payload_bytes;
                    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                        report.interrupted = true;
                        break;
                    }
                    reconcile_prepared(config, &prepared)
                }
            },
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
        PreparedMessage {
            message,
            body: "body\n".into(),
            sender: "BlueLake".into(),
            project_slug: "project".into(),
            recipients: vec!["GreenStone".into(), "RedFox".into()],
            payload_bytes: 512,
            archive_metadata_known: false,
        }
    }

    #[test]
    fn enable_override_is_explicit_and_invalid_values_fail_disabled() {
        assert!(parse_enabled(None));
        for raw in ["true", " 1 ", "YES", "on"] {
            assert!(parse_enabled(Some(raw)));
        }
        for raw in ["false", "0", "off", "no", "", "typo"] {
            assert!(!parse_enabled(Some(raw)));
        }
        let config = Config {
            database_url: "sqlite:///:memory:".into(),
            ..Config::default()
        };
        assert!(
            !enabled(&config),
            "ephemeral mailboxes do not start archive maintenance"
        );
    }

    #[test]
    fn surviving_reply_metadata_is_preserved_but_conflicting_fields_refuse() {
        let original = prepared();
        let mut message = original.message.clone();
        message["reply_to"] = json!(7);
        message["future_metadata"] = json!({"opaque": true});
        validate_surviving_message(&original, &message, &original.body).unwrap();
        for (key, value) in [
            ("id", json!(10)),
            ("from", json!("RedFox")),
            ("to", json!(["RedFox"])),
            ("bcc", json!([])),
            ("project", json!("/other")),
            ("reply_to", json!(9)),
            ("created", json!("invalid")),
        ] {
            let mut changed = message.clone();
            changed[key] = value;
            assert!(
                validate_surviving_message(&original, &changed, &original.body).is_err(),
                "{key}"
            );
        }
        assert!(validate_surviving_message(&original, &message, "different body").is_err());
        assert_eq!(message["reply_to"], 7);
    }

    #[test]
    fn unarchived_thread_is_deferred_instead_of_inventing_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(error.contains("reply metadata cannot be inferred"));
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        assert!(!paths.canonical.exists());
    }

    #[test]
    fn inbox_only_reply_recovery_preserves_parent_extensions_and_bcc_privacy() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, "project").unwrap();
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
        let bytes = crate::render_message_bundle_content(&redacted, &original.body).unwrap();
        crate::ensure_parent_dir(&paths.inbox[0]).unwrap();
        std::fs::write(&paths.inbox[0], bytes.as_bytes()).unwrap();

        let repaired = reconcile_prepared(&config, &original).unwrap();
        assert_eq!(repaired.files_created, 3);
        assert!(repaired.git_commit_needed);
        for path in [&paths.canonical, &paths.outbox] {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, full);
            assert_eq!(body, original.body);
        }
        for path in &paths.inbox {
            assert_eq!(std::fs::read(path).unwrap(), bytes.as_bytes());
            let (message, _) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message["bcc"], json!([]));
            assert_eq!(message["reply_to"], 7);
        }
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        assert_eq!(
            reconcile_prepared(&config, &original).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }

    #[test]
    fn conflicting_inbox_reply_parents_preserve_all_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut originals = Vec::new();
        for (index, path) in paths.inbox.iter().enumerate() {
            let mut message = crate::redact_message_bcc_for_inbox(&original.message);
            message["reply_to"] = json!(7 + index);
            let bytes = crate::render_message_bundle_content(&message, &original.body).unwrap();
            crate::ensure_parent_dir(path).unwrap();
            std::fs::write(path, bytes.as_bytes()).unwrap();
            originals.push(bytes);
        }
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(error.contains("inbox metadata disagree"), "{error}");
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        for (path, bytes) in paths.inbox.iter().zip(originals) {
            assert_eq!(std::fs::read(path).unwrap(), bytes.as_bytes());
        }
    }

    #[test]
    fn inbox_recovery_refuses_private_routing_and_malformed_redaction() {
        let original = prepared();
        let redacted = crate::redact_message_bcc_for_inbox(&original.message);
        for bcc in [json!(["RedFox"]), json!(null), json!("RedFox")] {
            let mut message = redacted.clone();
            message["bcc"] = bcc;
            let error = restore_inbox_metadata(&original, message, &original.body).unwrap_err();
            assert!(error.contains("BCC redaction"), "{error}");
        }
        let mut missing = redacted;
        missing.as_object_mut().unwrap().remove("bcc");
        assert!(restore_inbox_metadata(&original, missing, &original.body).is_err());
    }

    #[test]
    fn inbox_recovery_still_requires_live_identity_body_and_visible_routing() {
        let original = prepared();
        let redacted = crate::redact_message_bcc_for_inbox(&original.message);
        for (key, value) in [
            ("id", json!(10)),
            ("project_slug", json!("other-project")),
            ("to", json!(["RedFox"])),
            ("cc", json!(["RedFox"])),
            ("reply_to", json!(9)),
        ] {
            let mut message = redacted.clone();
            message[key] = value;
            assert!(
                restore_inbox_metadata(&original, message, &original.body).is_err(),
                "{key}"
            );
        }
        assert!(restore_inbox_metadata(&original, redacted, "different body").is_err());
    }

    /// Store message artifacts only in Git, never in the working tree. Existing
    /// tree entries and parent commits remain intact throughout these fixtures.
    fn commit_survivors(
        archive: &ProjectArchive,
        files: &[(&std::path::Path, &Value)],
        body: &str,
    ) -> git2::Oid {
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let baseline = match &parent {
            Some(parent) => parent.tree().unwrap(),
            None => {
                let id = repo.treebuilder(None).unwrap().write().unwrap();
                repo.find_tree(id).unwrap()
            }
        };
        let mut updates = git2::build::TreeUpdateBuilder::new();
        for (path, message) in files {
            let bytes = crate::render_message_bundle_content(message, body).unwrap();
            let blob = repo.blob(bytes.as_bytes()).unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
            updates.upsert(relative.as_str(), blob, git2::FileMode::Blob);
        }
        let id = updates.create_updated(&repo, &baseline).unwrap();
        let tree = repo.find_tree(id).unwrap();
        let signature = git2::Signature::now("reconcile-test", "reconcile@test.invalid").unwrap();
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "surviving mail",
            &tree,
            &parents,
        )
        .unwrap()
    }

    pub(super) fn git_fixture() -> (tempfile::TempDir, Config, PreparedMessage, ProjectArchive) {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, &original.project_slug).unwrap();
        (temp, config, original, archive)
    }

    #[test]
    fn committed_inbox_only_restores_all_copies_and_is_idempotent() {
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
        let source = commit_survivors(
            &archive,
            &[
                (paths.inbox[0].as_path(), &redacted),
                (paths.inbox[1].as_path(), &redacted),
            ],
            &original.body,
        );
        for path in [&paths.canonical, &paths.outbox]
            .into_iter()
            .chain(paths.inbox.iter())
        {
            assert!(!path.exists());
        }
        let result = reconcile_prepared(&config, &original).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        for path in [&paths.canonical, &paths.outbox] {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, full);
            assert_eq!(body, original.body);
        }
        for path in &paths.inbox {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, redacted);
            assert_eq!(body, original.body);
            assert_eq!(message["bcc"], json!([]));
        }
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert!(repo.find_commit(source).is_ok());
        let head = repo.head().unwrap().target().unwrap();
        assert_eq!(
            reconcile_prepared(&config, &original).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
    }

    #[test]
    fn conflicting_committed_full_copies_are_not_silently_selected() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = original.message.clone();
        first["reply_to"] = json!(7);
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        let head = commit_survivors(
            &archive,
            &[
                (paths.canonical.as_path(), &first),
                (paths.outbox.as_path(), &second),
            ],
            &original.body,
        );
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("committed canonical and outbox metadata disagree"),
            "{error}"
        );
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
    }

    #[test]
    fn conflicting_committed_inbox_parents_preserve_head_and_worktree() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = crate::redact_message_bcc_for_inbox(&original.message);
        first["reply_to"] = json!(7);
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        let head = commit_survivors(
            &archive,
            &[
                (paths.inbox[0].as_path(), &first),
                (paths.inbox[1].as_path(), &second),
            ],
            &original.body,
        );
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("committed inbox metadata disagree"),
            "{error}"
        );
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        for path in [&paths.canonical, &paths.outbox]
            .into_iter()
            .chain(paths.inbox.iter())
        {
            assert!(!path.exists());
        }
    }

    #[test]
    fn committed_inbox_cannot_supply_bcc_or_a_different_body() {
        for wrong_body in [false, true] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let mut message = crate::redact_message_bcc_for_inbox(&original.message);
            message["reply_to"] = json!(7);
            if !wrong_body {
                message["bcc"] = json!(["RedFox"]);
            }
            let body = if wrong_body {
                "different"
            } else {
                &original.body
            };
            let head = commit_survivors(&archive, &[(paths.inbox[0].as_path(), &message)], body);
            let error = reconcile_prepared(&config, &original).unwrap_err();
            let expected = if wrong_body {
                "body conflicts"
            } else {
                "BCC redaction"
            };
            assert!(error.contains(expected), "{error}");
            let repo = git2::Repository::open(&archive.repo_root).unwrap();
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
            assert!(!paths.canonical.exists());
        }
    }

    #[test]
    fn committed_message_snapshot_does_not_follow_head_advancement() {
        let (_temp, _config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = crate::redact_message_bcc_for_inbox(&original.message);
        first["reply_to"] = json!(7);
        commit_survivors(
            &archive,
            &[(paths.inbox[0].as_path(), &first)],
            &original.body,
        );
        let snapshot = CommittedMessages::open(&archive).unwrap();
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        commit_survivors(
            &archive,
            &[(paths.inbox[0].as_path(), &second)],
            &original.body,
        );
        let (observed, _) = snapshot.read(&archive, &paths.inbox[0]).unwrap().unwrap();
        assert_eq!(observed, first);
        let (observed, _) = CommittedMessages::open(&archive)
            .unwrap()
            .read(&archive, &paths.inbox[0])
            .unwrap()
            .unwrap();
        assert_eq!(observed, second);
    }

    #[test]
    fn payload_admission_distinguishes_fresh_batch_from_impossible_payload() {
        assert_eq!(
            payload_admission(0, MAX_BATCH_PAYLOAD_BYTES),
            PayloadAdmission::Fits
        );
        assert_eq!(
            payload_admission(1, MAX_BATCH_PAYLOAD_BYTES),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(0, MAX_BATCH_PAYLOAD_BYTES + 1),
            PayloadAdmission::Oversized
        );
        assert_eq!(
            payload_admission(MAX_BATCH_PAYLOAD_BYTES, 1),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(usize::MAX, usize::MAX),
            PayloadAdmission::Oversized
        );
        assert_eq!(
            payload_admission(usize::MAX, 1),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(MAX_BATCH_PAYLOAD_BYTES - 1, 1),
            PayloadAdmission::Fits
        );
    }

    #[test]
    fn json_expansion_can_exceed_batch_limit_without_exceeding_sql_input_limit() {
        let subject = "\u{0001}".repeat(3 * 1024 * 1024);
        assert!(subject.len() < usize::try_from(MAX_DB_PAYLOAD_BYTES).unwrap());
        let payload = json!({"subject": subject}).to_string();
        assert!(payload.len() > MAX_BATCH_PAYLOAD_BYTES);
        assert_eq!(
            payload_admission(0, payload.len()),
            PayloadAdmission::Oversized
        );
    }

    #[test]
    fn lane_order_survives_single_payload_batch_budgets() {
        let mut cursor = ReconcileCursor::default();
        let mut tail_next = 1000;
        let mut history_next = 999;
        let mut tail_count = 0;
        let mut history_count = 0;
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let ids = interleave_ids(&[tail_next], &[history_next], cursor.next_lane_is_history);
            let (id, tail) = ids[0];
            assert!(seen.insert(id));
            assert_eq!(
                payload_admission(0, MAX_BATCH_PAYLOAD_BYTES),
                PayloadAdmission::Fits
            );
            assert_eq!(
                payload_admission(MAX_BATCH_PAYLOAD_BYTES, 1),
                PayloadAdmission::NextBatch
            );
            // Exactly one item fits. The rejected second item must retain
            // priority across the next selection, not be perpetually second.
            cursor.advance(id, tail);
            if tail {
                tail_count += 1;
                tail_next += 1;
            } else {
                history_count += 1;
                history_next -= 1;
            }
        }
        assert_eq!((tail_count, history_count), (50, 50));
        assert_eq!(cursor.tail_after, Some(1049));
        assert_eq!(cursor.backfill_ceiling, Some(949));
    }

    #[test]
    fn interleaving_preserves_each_lane_and_duplicate_cursor_updates() {
        assert_eq!(
            interleave_ids(&[10, 11], &[9, 8, 7], true),
            vec![(9, false), (10, true), (8, false), (11, true), (7, false)]
        );
        assert_eq!(
            interleave_ids(&[], &[9, 8], false),
            vec![(9, false), (8, false)]
        );
        assert_eq!(
            interleave_ids(&[10, 11], &[], true),
            vec![(10, true), (11, true)]
        );
        let mut cursor = ReconcileCursor::default();
        let mut processed = HashSet::new();
        for (id, tail) in interleave_ids(&[9], &[9], false) {
            processed.insert(id);
            cursor.advance(id, tail);
        }
        assert_eq!(processed.len(), 1);
        assert_eq!(cursor.tail_after, Some(9));
        assert_eq!(cursor.backfill_ceiling, Some(8));
    }

    #[test]
    fn oversized_projection_does_not_pin_later_mail_and_preserves_source() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            for id in [901, 902] {
                conn.execute_raw(&format!(
                    "INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES({id}, 101, 101, 'small', 'body', 'normal', 0, 1000000, '{{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[]}}', '[]')"
                )).unwrap();
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES({id}, 102, 'to')"
                )).unwrap();
            }
            // Literal contains no SQL quotes; control characters are stored raw
            // and expand sixfold only when the archive JSON is serialized.
            let subject = "\u{0001}".repeat(3 * 1024 * 1024);
            conn.execute_raw(&format!(
                "UPDATE messages SET subject = '{subject}' WHERE id = 901"
            ))
            .unwrap();
            drop(conn);
            std::fs::create_dir_all(pool.storage_root()).unwrap();
            let config = Config {
                database_url,
                storage_root: pool.storage_root().to_path_buf(),
                ..Config::default()
            };
            let oversized = prepare_message(&cx, &pool, 901).unwrap();
            assert!(oversized.payload_bytes > MAX_BATCH_PAYLOAD_BYTES);
            let mut cursor = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                ..Default::default()
            };
            let report =
                reconcile_message_batch(&cx, &pool, &config, &mut cursor, &AtomicBool::new(false))
                    .unwrap();
            assert_eq!(report.scanned, 2);
            assert_eq!(report.deferred, 1);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 3);
            assert!(!report.budget_exhausted);
            assert_eq!(cursor.tail_after, Some(902));
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            let rows = conn
                .query_sync("SELECT subject FROM messages WHERE id = 901", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<String>("subject").unwrap(), subject);
            let receipts = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 902",
                    &[],
                )
                .unwrap();
            assert_eq!(
                receipts[0].get_named::<Option<i64>>("read_ts").unwrap(),
                None
            );
            assert_eq!(
                receipts[0].get_named::<Option<i64>>("ack_ts").unwrap(),
                None
            );
        });
    }

    #[test]
    fn real_file_backed_projection_repairs_without_modifying_mailbox_rows() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let pool_config = mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            };
            let pool = mcp_agent_mail_db::create_pool(&pool_config).unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
                VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                VALUES(901, 101, 101, 'handoff', 'body', 'normal', 0, 1000000, '{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[]}', '[]')").unwrap();
            conn.execute_raw(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 102, 'to')",
            )
            .unwrap();
            let before = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(before[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(before[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            drop(conn);
            // Source admission canonicalizes the archive root before checking
            // query-only mode. Match server initialization so this fixture
            // reaches the intended refusal instead of failing on a missing root.
            std::fs::create_dir_all(pool.storage_root()).unwrap();
            let config = Config {
                storage_root: pool.storage_root().to_path_buf(),
                database_url,
                ..Config::default()
            };
            let stop = AtomicBool::new(false);
            let readonly = DbPool::new_query_only(&pool_config).unwrap();
            let rejected = reconcile_message_batch(
                &cx,
                &readonly,
                &config,
                &mut ReconcileCursor::default(),
                &stop,
            )
            .unwrap_err();
            assert!(rejected.contains("query-only snapshots"), "{rejected}");
            drop(readonly);
            let mut cursor = ReconcileCursor::default();
            let report = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
            assert_eq!(report.scanned, 1);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 3);
            assert_eq!(report.deferred, 0);
            let again = reconcile_message_batch(
                &cx,
                &pool,
                &config,
                &mut ReconcileCursor::default(),
                &stop,
            )
            .unwrap();
            assert_eq!(again.repaired, 0);
            assert_eq!(again.unchanged, 1);
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            let after = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(after[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(after[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            let rows = conn
                .query_sync("SELECT body_md FROM messages WHERE id = 901", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<String>("body_md").unwrap(), "body");
            drop(conn);
            stop.store(true, Ordering::Release);
            let stopped = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.scanned, 0);
            let other = temp.path().join("not-the-live-db.sqlite3");
            std::fs::write(&other, b"preserve other database bytes").unwrap();
            let mut mismatched = config.clone();
            mismatched.database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&other);
            assert!(
                validate_pool_binding(&pool, &mismatched)
                    .unwrap_err()
                    .contains("not the configured live database")
            );
            assert_eq!(
                std::fs::read(&other).unwrap(),
                b"preserve other database bytes"
            );
        });
    }
}
