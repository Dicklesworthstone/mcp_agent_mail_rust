//! One bounded mailbox observation for archive recovery, including legacy mail.
//!
//! The recipient cache is a rendering optimization, not the only durable copy
//! of a delivery. Legacy rows may still have its original `{}` default. Keep
//! message metadata and the recipient rows in one SQL statement, without
//! multiplying a large message body by its number of recipients.

use std::collections::HashSet;

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_db::sqlmodel_core::{Row, Value as SqlValue};
use mcp_agent_mail_db::{DbConn, DbPool};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::MAX_RECIPIENTS;
#[cfg(test)]
use super::outcome;
use super::{MAX_DB_PAYLOAD_BYTES, PreparedMessage, ReconcileFailure, source_error};

const MAX_RECIPIENT_NAME_BYTES: i64 = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveMetadata {
    // Absence proves a send. An explicit null, duplicate key, unknown field,
    // string or fractional number is conflicting evidence, never that absence.
    #[serde(default, deserialize_with = "deserialize_reply_parent")]
    reply_to: Option<i64>,
}

fn deserialize_reply_parent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    i64::deserialize(deserializer).map(Some)
}

fn decode_archive_metadata(
    raw: &str,
    id: i64,
    thread_id: Option<&str>,
) -> Result<Option<i64>, String> {
    // Serde's derived struct visitor also accepts sequence encodings.
    // Durable metadata must be an object; [] cannot prove a fresh send.
    if !raw.trim_start().starts_with('{') {
        return Err("message archive metadata is not an object; repair deferred".to_string());
    }
    let metadata: ArchiveMetadata = serde_json::from_str(raw).map_err(|_| {
        "message archive metadata is not a supported reply record; repair deferred".to_string()
    })?;
    if metadata
        .reply_to
        .is_some_and(|parent| parent <= 0 || parent == id || thread_id.is_none_or(str::is_empty))
    {
        return Err("message archive metadata has an invalid reply parent or thread".to_string());
    }
    Ok(metadata.reply_to)
}

// UNION ALL gives the message one row and each delivery one small row. Both
// arms belong to the same statement/snapshot. LEFT JOIN is intentional: a
// missing recipient identity must be reported, not silently dropped by a join.
// The extra row beyond the recipient limit is an overflow witness, not mail
// that the recovery worker is allowed to discard.
const SOURCE_SQL: &str = mcp_agent_mail_db::queries::MESSAGE_ARCHIVE_SOURCE_SQL;

#[cfg(test)]
pub(super) fn prepare_message(cx: &Cx, pool: &DbPool, id: i64) -> Result<PreparedMessage, String> {
    prepare_message_classified(cx, pool, id).map_err(|error| error.detail)
}

pub(super) fn prepare_message_classified(
    cx: &Cx,
    pool: &DbPool,
    id: i64,
) -> Result<PreparedMessage, ReconcileFailure> {
    with_source_connection(cx, pool, |conn| {
        read_source(id, |sql, params| {
            conn.query_sync(sql, params)
                .map_err(ReconcileFailure::source_unavailable)
        })
    })
}

/// A retry must not reacquire the reader whose SQL operation just failed.
/// The maintenance pool has one connection: returning an obsolete pager to it
/// can turn the bounded retry lane into repeated reads of the same bad handle.
/// Retire only failed source reads, not valid reads that expose conflicting
/// durable metadata. Neither path mutates mailbox rows or relaxes validation.
pub(super) fn with_source_connection<T>(
    cx: &Cx,
    pool: &DbPool,
    read: impl FnOnce(&DbConn) -> Result<T, ReconcileFailure>,
) -> Result<T, ReconcileFailure> {
    let conn = match block_on(pool.acquire(cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Err(ReconcileFailure::source_unavailable(error)),
        Outcome::Cancelled(_) => {
            return Err(ReconcileFailure::source_unavailable(
                "source read cancelled",
            ));
        }
        Outcome::Panicked(_) => {
            return Err(ReconcileFailure::source_unavailable("source read panicked"));
        }
    };
    // The synchronous driver/decoder may unwind instead of returning an SQL
    // error. Keep that failure inside the read-only observation: the caller's
    // bounded retry lane must retain the message ID, and the uncertain pager
    // must never return to this one-slot pool. No archive publication or SQL
    // mutation is included in this boundary. Do not copy the panic payload
    // into the returned error; it may contain private source data.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(&conn)))
        .unwrap_or_else(|_| {
            Err(ReconcileFailure::source_unavailable(
                "message archive source read panicked",
            ))
        });
    if result.as_ref().is_err_and(|error| error.transient) {
        // Detach updates pool accounting and prevents Drop from returning this
        // handle to the idle slot. Use the driver's normal explicit close path;
        // do not clear shared pools, change generations, or repair the database.
        mcp_agent_mail_db::close_db_conn(conn.detach(), "failed message archive source read");
    }
    result
}

fn read_source(
    id: i64,
    query: impl FnOnce(&str, &[SqlValue]) -> Result<Vec<Row>, ReconcileFailure>,
) -> Result<PreparedMessage, ReconcileFailure> {
    // The pinned runtime's compound-select executor applies LIMIT without
    // bindings. Only the fixed internal row budget is rendered as SQL; message
    // IDs and payload bounds remain bound, with explicit indices across arms.
    let limit = MAX_RECIPIENTS + 2;
    let sql = format!("{SOURCE_SQL} LIMIT {limit}");
    let rows = query(
        &sql,
        &[
            id.into(),
            MAX_DB_PAYLOAD_BYTES.into(),
            MAX_RECIPIENT_NAME_BYTES.into(),
        ],
    )?;
    prepare_source_rows(id, &rows).map_err(ReconcileFailure::from)
}

/// Retention verifies the exact projection captured by the database, then the
/// deletion transaction rechecks that same projection. A second independent
/// read here could otherwise verify different content from the deletion witness.
pub(super) fn prepare_source_rows(id: i64, rows: &[Row]) -> Result<PreparedMessage, String> {
    let row = rows
        .first()
        .filter(|row| row.get_named::<i64>("row_kind").ok() == Some(0))
        .ok_or_else(|| {
            "message missing, oversized, or lacking an unambiguous project/sender".to_string()
        })?;
    if id <= 0 || row.get_named::<i64>("id").map_err(source_error)? != id {
        return Err("message source identity does not match the requested ID".to_string());
    }
    let text = |name| row.get_named::<String>(name).map_err(source_error);
    let body = text("body_md")?;
    let sender = text("sender")?;
    let project_slug = text("project_slug")?;
    let project_id = row.get_named::<i64>("project_id").map_err(source_error)?;
    let created = row.get_named::<i64>("created_ts").map_err(source_error)?;
    if chrono::DateTime::from_timestamp_micros(created).is_none() {
        return Err("message creation timestamp cannot be represented faithfully".to_string());
    }
    let ack = row.get_named::<i64>("ack_required").map_err(source_error)?;
    if !matches!(ack, 0 | 1) {
        return Err("message acknowledgment flag is not boolean".to_string());
    }
    let cached: Value = serde_json::from_str(&text("recipients_json")?)
        .map_err(|_| "message recipient metadata is not valid JSON".to_string())?;
    // Both the legacy fallback and a populated cache require complete delivery
    // authority. Otherwise a stale cache could fabricate an inbox or expose a
    // BCC recipient as TO/CC when the archive is rebuilt.
    let durable = routing_from_rows(&rows[1..], id, project_id)?;
    let routing = select_routing(cached, &durable)?;
    let attachments: Value = serde_json::from_str(&text("attachments")?)
        .map_err(|_| "message attachment metadata is not valid JSON".to_string())?;
    if !attachments.is_array() {
        return Err("message attachment metadata is not an array".to_string());
    }
    let recipients = routing_names(&routing)?;
    let mut message = json!({
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
    let archive_metadata = row
        .get_named::<Option<String>>("archive_metadata_json")
        .map_err(source_error)?;
    if let Some(metadata) = &archive_metadata
        && let Some(parent) = decode_archive_metadata(metadata, id, message["thread_id"].as_str())?
    {
        message["reply_to"] = json!(parent);
    }
    let payload_bytes = body.len().saturating_add(message.to_string().len());
    Ok(PreparedMessage {
        message,
        body,
        sender,
        project_slug,
        recipients,
        payload_bytes,
        archive_metadata_known: archive_metadata.is_some(),
    })
}

fn select_routing(cached: Value, durable: &Value) -> Result<Value, String> {
    // Only the exact legacy default authorizes reconstruction of the cache.
    // A malformed/partial populated cache is conflicting evidence, not absence.
    if cached.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(durable.clone());
    }
    let _ = routing_names(&cached)?;
    for kind in ["to", "cc", "bcc"] {
        let names = |routing: &Value| -> Result<Vec<String>, String> {
            let mut names = routing
                .get(kind)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("message recipient metadata lacks {kind} array"))?
                .iter()
                .map(|name| {
                    name.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "non-string message recipient".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Do not deduplicate: repeated names are not a faithful delivery
            // set, including a duplicate hidden among otherwise valid roles.
            names.sort_unstable();
            Ok(names)
        };
        if names(&cached)? != names(durable)? {
            return Err(format!(
                "cached {kind} routing conflicts with durable delivery rows; archive repair refused"
            ));
        }
    }
    // Keep original order and spelling after proving role-by-role equality.
    // Normalizing a valid cache here can conflict with surviving archive bytes.
    Ok(cached)
}

/// The agent inboxes a routing delivers to. A project shared-mailbox address
/// (GH#282) is routing but never an inbox, so it is counted, not returned.
fn routing_names(routing: &Value) -> Result<Vec<String>, String> {
    let mut recipients = Vec::new();
    let mut addressed = 0usize;
    for kind in ["to", "cc", "bcc"] {
        for name in routing
            .get(kind)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("message recipient metadata lacks {kind} array"))?
        {
            let name = name
                .as_str()
                .ok_or_else(|| "non-string message recipient".to_string())?;
            crate::validate_archive_component("recipient", name)
                .map_err(|error| error.to_string())?;
            addressed += 1;
            if addressed > MAX_RECIPIENTS {
                return Err("message recipient budget exceeded".to_string());
            }
            if mcp_agent_mail_db::project_mailbox::parse_project_mailbox_address(name).is_some() {
                if kind == "bcc" {
                    return Err("a project mailbox cannot be a bcc recipient".to_string());
                }
                continue;
            }
            recipients.push(name.to_string());
        }
    }
    if addressed == 0 {
        return Err("message has no authoritative recipients".to_string());
    }
    recipients.sort_unstable();
    recipients.dedup();
    Ok(recipients)
}

fn routing_from_rows(rows: &[Row], id: i64, project_id: i64) -> Result<Value, String> {
    if rows.is_empty() || rows.len() > MAX_RECIPIENTS {
        return Err("message recipient set is empty or exceeds its bound".to_string());
    }
    let mut routing = json!({"to": [], "cc": [], "bcc": []});
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for row in rows {
        let integer = |key| row.get_named::<i64>(key).map_err(source_error);
        let row_kind = integer("row_kind")?;
        if !matches!(row_kind, 1 | 2) || integer("id")? != id {
            return Err("recipient projection has a mismatched message identity".to_string());
        }
        if row_kind == 2 {
            // GH#282: the project shared-mailbox delivery, named by address.
            if integer("recipient_project_id")? != project_id {
                return Err(
                    "project mailbox delivery belongs to a different project; archive repair refused"
                        .to_string(),
                );
            }
            let address = row
                .get_named::<Option<String>>("recipient_name")
                .map_err(source_error)?
                .filter(|name| {
                    mcp_agent_mail_db::project_mailbox::parse_project_mailbox_address(name)
                        .is_some_and(|ident| !ident.is_empty())
                })
                .ok_or_else(|| "project mailbox delivery has no valid address".to_string())?;
            let kind = row
                .get_named::<Option<String>>("recipient_kind")
                .map_err(source_error)?
                .filter(|kind| matches!(kind.as_str(), "to" | "cc"))
                .ok_or_else(|| "project mailbox delivery has an invalid kind".to_string())?;
            routing[&kind]
                .as_array_mut()
                .ok_or_else(|| "recipient routing is not an array".to_string())?
                .push(Value::String(address));
            continue;
        }
        let agent_id = integer("recipient_id")?;
        if agent_id <= 0 || !ids.insert(agent_id) {
            return Err("recipient projection has duplicate or invalid identities".to_string());
        }
        // A foreign sender is valid for a contact notice. A foreign recipient
        // is not authority to create an inbox under this message's project.
        if integer("recipient_project_id")? != project_id {
            return Err(
                "recipient belongs to a different project; archive repair refused".to_string(),
            );
        }
        let name = row
            .get_named::<Option<String>>("recipient_name")
            .map_err(source_error)?
            .ok_or_else(|| "recipient identity is missing or oversized".to_string())?;
        crate::validate_archive_component("recipient", &name).map_err(|error| error.to_string())?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err("recipient names do not identify unique archive inboxes".to_string());
        }
        let kind = row
            .get_named::<Option<String>>("recipient_kind")
            .map_err(source_error)?
            .filter(|kind| matches!(kind.as_str(), "to" | "cc" | "bcc"))
            .ok_or_else(|| "recipient has an invalid delivery kind".to_string())?;
        routing[&kind]
            .as_array_mut()
            .ok_or_else(|| "recipient routing is not an array".to_string())?
            .push(Value::String(name));
    }
    // Source rows are ordered by recipient ID, so fallback serialization is
    // stable across passes without changing the authoritative delivery kind.
    Ok(routing)
}

#[cfg(test)]
mod tests {
    use super::super::{
        ReconcileCursor, ReconcileResult, read_surviving_message, reconcile_message_batch,
        reconcile_prepared,
    };
    use super::*;
    use mcp_agent_mail_core::Config;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    // The pre-GH#354 statement is an independent result/cost control. Keeping
    // it literal prevents an accidental change to the production projection
    // or byte bound from silently changing both sides of the comparison.
    const LEGACY_SOURCE_SQL: &str = "\
SELECT 0 AS row_kind, m.id, m.project_id, m.subject, m.body_md, m.thread_id, m.topic, \
       m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
       m.archive_metadata_json, \
       p.slug AS project_slug, p.human_key AS project_key, a.name AS sender, \
       NULL AS recipient_id, NULL AS recipient_project_id, \
       NULL AS recipient_name, NULL AS recipient_kind, \
       m.sender_id, a.project_id AS sender_project_id, \
       NULL AS recipient_read_ts, NULL AS recipient_ack_ts \
FROM messages m JOIN projects p ON p.id = m.project_id \
JOIN agents a ON a.id = m.sender_id \
WHERE m.id = ?1 AND \
      length(CAST(m.body_md AS BLOB)) + length(CAST(m.subject AS BLOB)) + \
      length(CAST(m.recipients_json AS BLOB)) + length(CAST(m.attachments AS BLOB)) + \
      length(CAST(m.importance AS BLOB)) + length(CAST(p.slug AS BLOB)) + \
      length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
      COALESCE(length(CAST(m.thread_id AS BLOB)), 0) + \
      COALESCE(length(CAST(m.topic AS BLOB)), 0) + \
      COALESCE(length(CAST(m.archive_metadata_json AS BLOB)), 0) <= ?2 \
UNION ALL \
SELECT 1, mr.message_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
       NULL, NULL, NULL, mr.agent_id, a.project_id, \
       CASE WHEN length(CAST(a.name AS BLOB)) <= ?3 THEN a.name ELSE NULL END, \
       CASE WHEN length(CAST(mr.kind AS BLOB)) <= 3 THEN mr.kind ELSE NULL END, \
       NULL, NULL, \
       CASE WHEN mr.read_ts IS NULL OR typeof(mr.read_ts) = 'integer' \
            THEN mr.read_ts ELSE 'invalid_receipt' END, \
       CASE WHEN mr.ack_ts IS NULL OR typeof(mr.ack_ts) = 'integer' \
            THEN mr.ack_ts ELSE 'invalid_receipt' END \
FROM message_recipients mr LEFT JOIN agents a ON a.id = mr.agent_id \
WHERE mr.message_id = ?1 \
UNION ALL \
SELECT 2, d.message_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
       NULL, NULL, NULL, NULL, d.project_id, \
       CASE WHEN length(CAST(p.slug AS BLOB)) <= ?3 THEN 'project:' || p.slug ELSE NULL END, \
       CASE WHEN length(CAST(d.kind AS BLOB)) <= 3 THEN d.kind ELSE NULL END, \
       NULL, NULL, NULL, NULL \
FROM project_mailbox_deliveries d LEFT JOIN projects p ON p.id = d.project_id \
WHERE d.message_id = ?1 \
ORDER BY row_kind, recipient_id";

    fn assert_source_rows_equal(expected: &[Row], actual: &[Row]) {
        assert_eq!(actual.len(), expected.len());
        for (expected, actual) in expected.iter().zip(actual) {
            for column in 0..24 {
                assert!(expected.get(column).is_some());
                assert_eq!(actual.get(column), expected.get(column), "column {column}");
            }
            assert!(actual.get(24).is_none());
        }
    }

    fn source_sql_with_limit(sql: &str) -> String {
        format!("{sql} LIMIT {}", MAX_RECIPIENTS + 2)
    }

    // Includes the driver's worker threads, unlike calling-thread CPU clocks.
    // These diagnostic counters require an isolated test invocation for useful
    // attribution; they are not noisy wall-time assertions in the test gate.
    fn process_cpu_ticks() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        let mut fields = fields.split_whitespace().skip(11);
        let user = fields.next()?.parse::<u64>().ok()?;
        let system = fields.next()?.parse::<u64>().ok()?;
        user.checked_add(system)
    }

    fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let database_url =
                mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3"));
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1), (201, 'foreign', '/foreign', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1), (103, 101, 'RedFox', 'test', 'test', '', 1, 1), (104, 101, 'GoldLeaf', 'test', 'test', '', 1, 1), (201, 201, 'OrangeHill', 'test', 'test', '', 1, 1), (202, 201, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES(901, 101, 201, 'Contact handoff', 'Keep this body.', 'normal', 1, 1000000, '{}', '[]')").unwrap();
            conn.execute_raw("INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) VALUES(901, 102, 'to', 7, 11), (901, 103, 'bcc', NULL, NULL), (901, 104, 'cc', NULL, NULL)").unwrap();
            drop(conn);
            std::fs::create_dir_all(pool.storage_root()).unwrap();
            let config = Config {
                database_url,
                storage_root: pool.storage_root().to_path_buf(),
                ..Config::default()
            };
            test(&cx, &pool, &config);
        });
    }

    fn assert_exact_recovered_bundle(
        config: &Config,
        original: &PreparedMessage,
        parent: Option<i64>,
    ) {
        let archive = crate::ensure_archive(config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        assert!(paths.inbox.iter().all(|path| !path.exists()));
        let repaired = reconcile_prepared(config, original).unwrap();
        assert_eq!(repaired.files_created, 5);
        assert!(repaired.git_commit_needed);
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        for path in [&paths.canonical, &paths.outbox]
            .into_iter()
            .chain(&paths.inbox)
        {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(body, "Keep this exact body.\n");
            assert_eq!(message.get("reply_to").and_then(Value::as_i64), parent);
            assert_eq!(message["thread_id"], "conversation-root");
            if paths.inbox.contains(path) {
                assert_eq!(message["bcc"], json!([]));
            } else {
                assert_eq!(message["bcc"], json!(["RedFox"]));
            }
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
            let entry = tree.get_path(std::path::Path::new(&relative)).unwrap();
            assert_eq!(
                repo.find_blob(entry.id()).unwrap().content(),
                std::fs::read(path).unwrap()
            );
        }
        assert_eq!(
            reconcile_prepared(config, original).unwrap(),
            ReconcileResult::default()
        );
    }

    #[test]
    fn durable_reply_metadata_recovers_db_only_threaded_sends_and_replies() {
        fixture(|cx, pool, config| {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            for parent in [None, Some(901)] {
                let created = outcome(runtime.block_on(
                    mcp_agent_mail_db::queries::create_message_with_recipients_topic(
                        cx,
                        pool,
                        101,
                        101,
                        "Durable lineage",
                        "Keep this exact body.\n",
                        Some("conversation-root"),
                        Some("release"),
                        parent,
                        "normal",
                        true,
                        "[]",
                        &[(102, "to"), (103, "bcc"), (104, "cc")],
                        None,
                    ),
                ))
                .unwrap();
                let id = created.id.unwrap();
                // Reopen the real runtime driver: recovery must not depend on
                // the writer's pooled connection, returned row or archive queue.
                let reopened = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path()).unwrap();
                let original = read_source(id, |sql, params| {
                    reopened
                        .query_sync(sql, params)
                        .map_err(ReconcileFailure::source_unavailable)
                })
                .unwrap();
                drop(reopened);
                assert!(original.archive_metadata_known);
                assert_eq!(
                    original.message.get("reply_to").and_then(Value::as_i64),
                    parent
                );
                assert_eq!(original.message["thread_id"], "conversation-root");
                assert_exact_recovered_bundle(config, &original, parent);
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                let deliveries = conn
                    .query_sync(
                        "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                        &[id.into()],
                    )
                    .unwrap();
                assert_eq!(deliveries.len(), 3);
                for delivery in deliveries {
                    assert!(
                        delivery
                            .get_named::<Option<i64>>("read_ts")
                            .unwrap()
                            .is_none()
                    );
                    assert!(
                        delivery
                            .get_named::<Option<i64>>("ack_ts")
                            .unwrap()
                            .is_none()
                    );
                }
            }
        });
    }

    #[test]
    fn durable_tui_metadata_recovers_db_only_threaded_compose_after_reopen() {
        fixture(|_cx, pool, config| {
            let writer = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path()).unwrap();
            let id = mcp_agent_mail_db::sync::dispatch_root_message(
                &writer,
                "BlueLake",
                "TUI handoff",
                "Keep this exact body.\n",
                "high",
                Some("conversation-root"),
                &[
                    ("GreenStone".to_string(), "to".to_string()),
                    ("RedFox".to_string(), "bcc".to_string()),
                    ("GoldLeaf".to_string(), "cc".to_string()),
                ],
            )
            .unwrap();
            writer
                .execute_sync(
                    "UPDATE message_recipients SET read_ts = 7, ack_ts = 11 \
                     WHERE message_id = ? AND agent_id = 102",
                    &[id.into()],
                )
                .unwrap();
            drop(writer);

            // The TUI reports success after the database commit and does not
            // enqueue an archive bundle. Recover through a fresh file handle,
            // without the original connection or any surviving message copy.
            let reopened = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path()).unwrap();
            let prepared = read_source(id, |sql, params| {
                reopened
                    .query_sync(sql, params)
                    .map_err(ReconcileFailure::source_unavailable)
            })
            .unwrap();
            drop(reopened);
            assert!(prepared.archive_metadata_known);
            assert_eq!(prepared.message["thread_id"], "conversation-root");
            assert!(
                !prepared
                    .message
                    .as_object()
                    .unwrap()
                    .contains_key("reply_to")
            );
            assert_exact_recovered_bundle(config, &prepared, None);

            let verify = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path()).unwrap();
            let deliveries = verify
                .query_sync(
                    "SELECT agent_id, read_ts, ack_ts FROM message_recipients \
                     WHERE message_id = ? ORDER BY agent_id",
                    &[id.into()],
                )
                .unwrap();
            assert_eq!(deliveries.len(), 3);
            for delivery in deliveries {
                let recipient = delivery.get_named::<i64>("agent_id").unwrap();
                assert_eq!(
                    delivery.get_named::<Option<i64>>("read_ts").unwrap(),
                    (recipient == 102).then_some(7),
                );
                assert_eq!(
                    delivery.get_named::<Option<i64>>("ack_ts").unwrap(),
                    (recipient == 102).then_some(11),
                );
            }
        });
    }

    #[test]
    fn durable_attachment_content_digest_recovers_uncommitted_image_after_database_reopen() {
        fixture(|cx, pool, config| {
            let config = Config {
                keep_original_images: true,
                ..config.clone()
            };
            let archive = crate::ensure_archive(&config, "project").unwrap();
            let source = config.storage_root.join("accepted-image.png");
            image::RgbImage::new(2, 2).save(&source).unwrap();
            let original_bytes = std::fs::read(&source).unwrap();
            let stored =
                crate::store_attachment(&archive, &config, &source, crate::EmbedPolicy::File)
                    .unwrap();
            let webp_relative = stored.meta.path.as_ref().unwrap();
            let encoded_bytes = std::fs::read(archive.repo_root.join(webp_relative)).unwrap();
            let attachments = json!([stored.meta]);
            let repo = git2::Repository::open(&archive.repo_root).unwrap();
            assert!(
                repo.head()
                    .unwrap()
                    .peel_to_tree()
                    .unwrap()
                    .get_path(std::path::Path::new(webp_relative))
                    .is_err()
            );
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let created = outcome(runtime.block_on(
                mcp_agent_mail_db::queries::create_message_with_recipients_topic(
                    cx,
                    pool,
                    101,
                    101,
                    "Accepted image",
                    "Keep this exact body.\n",
                    Some("conversation-root"),
                    None,
                    None,
                    "normal",
                    true,
                    &attachments.to_string(),
                    &[(102, "to"), (103, "bcc"), (104, "cc")],
                    None,
                ),
            ))
            .unwrap();
            let reopened = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path()).unwrap();
            let prepared = read_source(created.id.unwrap(), |sql, params| {
                reopened
                    .query_sync(sql, params)
                    .map_err(ReconcileFailure::source_unavailable)
            })
            .unwrap();
            drop(reopened);
            assert_eq!(prepared.message["attachments"], attachments);
            assert_exact_recovered_bundle(&config, &prepared, None);
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            for (relative, expected) in [
                (webp_relative, &encoded_bytes),
                (stored.meta.original_path.as_ref().unwrap(), &original_bytes),
            ] {
                let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
                assert_eq!(
                    repo.find_blob(entry.id()).unwrap().content(),
                    expected.as_slice()
                );
                assert_eq!(
                    std::fs::read(archive.repo_root.join(relative)).unwrap(),
                    *expected
                );
            }
            assert_eq!(std::fs::read(&source).unwrap(), original_bytes);
        });
    }

    #[test]
    fn durable_reply_metadata_is_unchanged_by_idempotent_replay_or_conflict() {
        fixture(|cx, pool, config| {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let create = |fingerprint| {
                outcome(runtime.block_on(
                    mcp_agent_mail_db::queries::create_message_with_recipients_idempotent_topic(
                        cx,
                        pool,
                        101,
                        101,
                        "Reply once",
                        "Exact reply",
                        Some("root"),
                        None,
                        Some(901),
                        "normal",
                        false,
                        "[]",
                        &[(102, "to")],
                        None,
                        mcp_agent_mail_db::IdempotencyClaim {
                            project_id: 101,
                            tool: "reply_message",
                            key: "durable-reply",
                            fingerprint,
                        },
                    ),
                ))
                .unwrap()
            };
            let mcp_agent_mail_db::IdempotentOutcome::Fresh(original) = create("original") else {
                panic!("first reply must be fresh");
            };
            let mcp_agent_mail_db::IdempotentOutcome::Replayed(replayed) = create("original")
            else {
                panic!("same key must replay");
            };
            assert_eq!(replayed.id, original.id);
            assert!(matches!(
                create("different"),
                mcp_agent_mail_db::IdempotentOutcome::Conflict(_)
            ));
            let prepared = prepare_message(cx, pool, original.id.unwrap()).unwrap();
            assert!(prepared.archive_metadata_known);
            assert_eq!(prepared.message["reply_to"], 901);
            assert_eq!(
                reconcile_prepared(config, &prepared).unwrap().files_created,
                3
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT id, archive_metadata_json FROM messages ORDER BY id",
                    &[],
                )
                .unwrap();
            assert_eq!(rows.len(), 2, "replay and conflict must not add messages");
            assert_eq!(
                rows[1]
                    .get_named::<String>("archive_metadata_json")
                    .unwrap(),
                "{\"reply_to\":901}"
            );
        });
    }

    #[test]
    fn durable_reply_metadata_rejects_invalid_parent_without_consuming_message_id() {
        fixture(|cx, pool, _| {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            for (parent, thread) in [
                (0, Some("root")),
                (-1, Some("root")),
                (901, None),
                (901, Some("")),
                (902, Some("root")),
            ] {
                let error = outcome(runtime.block_on(
                    mcp_agent_mail_db::queries::create_message_with_recipients_topic(
                        cx,
                        pool,
                        101,
                        101,
                        "Rejected reply",
                        "body",
                        thread,
                        None,
                        Some(parent),
                        "normal",
                        false,
                        "[]",
                        &[(102, "to")],
                        None,
                    ),
                ))
                .unwrap_err();
                assert!(error.contains("reply_to"), "{error}");
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                let rows = conn
                    .query_sync("SELECT id FROM messages ORDER BY id", &[])
                    .unwrap();
                assert_eq!(rows.len(), 1, "invalid lineage must roll back the message");
                assert_eq!(rows[0].get_as::<i64>(0).unwrap(), 901);
            }
            let accepted = outcome(runtime.block_on(
                mcp_agent_mail_db::queries::create_message_with_recipients_topic(
                    cx,
                    pool,
                    101,
                    101,
                    "Fresh send",
                    "body",
                    Some("root"),
                    None,
                    None,
                    "normal",
                    false,
                    "[]",
                    &[(102, "to")],
                    None,
                ),
            ))
            .unwrap();
            assert_eq!(
                accepted.id,
                Some(902),
                "rejected self-parent must roll back ID election"
            );
        });
    }

    #[test]
    fn durable_reply_metadata_refuses_malformed_or_conflicting_authority() {
        fixture(|cx, pool, config| {
            for metadata in [
                "null",
                "[]",
                "broken",
                "{\"reply_to\":null}",
                "{\"reply_to\":0}",
                "{\"reply_to\":901}",
                "{\"reply_to\":\"7\"}",
                "{\"reply_to\":7,\"reply_to\":8}",
                "{\"reply_to\":7.5}",
                "{\"subject\":\"spoof\"}",
            ] {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_sync("UPDATE messages SET thread_id = 'root', archive_metadata_json = ? WHERE id = 901", &[metadata.into()]).unwrap();
                drop(conn);
                let error = prepare_message(cx, pool, 901).err().unwrap();
                assert!(error.contains("archive metadata"), "{metadata}: {error}");
            }
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET thread_id = 'root', archive_metadata_json = NULL WHERE id = 901").unwrap();
            drop(conn);
            let legacy = prepare_message(cx, pool, 901).unwrap();
            assert!(!legacy.archive_metadata_known);
            assert!(
                reconcile_prepared(config, &legacy)
                    .unwrap_err()
                    .contains("reply metadata cannot be inferred")
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE messages SET archive_metadata_json = '{\"reply_to\":7}' WHERE id = 901",
            )
            .unwrap();
            drop(conn);
            let prepared = prepare_message(cx, pool, 901).unwrap();
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &prepared.message,
                &prepared.sender,
                &prepared.recipients,
            )
            .unwrap()
            .0;
            let mut conflicting = prepared.message.clone();
            conflicting["reply_to"] = json!(8);
            let bytes = crate::render_message_bundle_content(&conflicting, &prepared.body).unwrap();
            crate::ensure_parent_dir(&paths.canonical).unwrap();
            std::fs::write(&paths.canonical, &bytes).unwrap();
            let error = reconcile_prepared(config, &prepared).unwrap_err();
            assert!(
                error.contains("conflicts with durable message metadata"),
                "{error}"
            );
            assert_eq!(std::fs::read_to_string(&paths.canonical).unwrap(), bytes);
            assert!(!paths.outbox.exists());
            assert!(paths.inbox.iter().all(|path| !path.exists()));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET archive_metadata_json = '{}' WHERE id = 901")
                .unwrap();
            drop(conn);
            let known_send = prepare_message(cx, pool, 901).unwrap();
            assert!(known_send.archive_metadata_known);
            assert!(
                !known_send
                    .message
                    .as_object()
                    .unwrap()
                    .contains_key("reply_to")
            );
            let error = reconcile_prepared(config, &known_send).unwrap_err();
            assert!(
                error.contains("conflicts with durable message metadata"),
                "{error}"
            );
            assert_eq!(std::fs::read_to_string(&paths.canonical).unwrap(), bytes);
            assert!(!paths.outbox.exists());
        });
    }

    #[test]
    fn legacy_cross_project_notice_restores_all_roles_without_redelivery() {
        fixture(|cx, pool, config| {
            let original = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(original.sender, "OrangeHill");
            assert_eq!(original.project_slug, "project");
            assert_eq!(original.message["to"], json!(["GreenStone"]));
            assert_eq!(original.message["cc"], json!(["GoldLeaf"]));
            assert_eq!(original.message["bcc"], json!(["RedFox"]));
            let repaired = reconcile_prepared(config, &original).unwrap();
            assert_eq!(repaired.files_created, 5);
            assert!(repaired.git_commit_needed);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            for path in [&paths.canonical, &paths.outbox] {
                let (message, body) = read_surviving_message(path).unwrap().unwrap();
                assert_eq!(message, original.message);
                assert_eq!(body, original.body);
            }
            for path in &paths.inbox {
                let (message, body) = read_surviving_message(path).unwrap().unwrap();
                assert_eq!(message["bcc"], json!([]));
                assert_eq!(body, original.body);
            }
            assert!(!config.storage_root.join("projects/foreign").exists());
            assert_eq!(
                reconcile_prepared(config, &original).unwrap(),
                ReconcileResult::default()
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT recipients_json FROM messages WHERE id = 901", &[])
                .unwrap();
            assert_eq!(
                rows[0].get_named::<String>("recipients_json").unwrap(),
                "{}"
            );
            let rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901 AND agent_id = 102",
                    &[],
                )
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 7);
            assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 11);
        });
    }

    #[test]
    fn populated_cache_still_allows_the_real_cross_project_sender() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\"],\"cc\":[\"GoldLeaf\"],\"bcc\":[\"RedFox\"]}' WHERE id = 901").unwrap();
            drop(conn);
            let message = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(message.sender, "OrangeHill");
            assert_eq!(message.message["bcc"], json!(["RedFox"]));
        });
    }

    #[test]
    fn legacy_routing_cannot_create_a_foreign_agents_local_inbox() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE message_recipients SET agent_id = 202 WHERE message_id = 901 AND agent_id = 102").unwrap();
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("different project"), "{error}");
            assert!(
                !config
                    .storage_root
                    .join("projects/project/messages")
                    .exists()
            );
        });
    }

    #[test]
    fn partial_cache_is_not_reinterpreted_as_the_legacy_default() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\"]}' WHERE id = 901",
            )
            .unwrap();
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("lacks cc array"), "{error}");
        });
    }

    #[test]
    fn legacy_message_without_delivery_rows_remains_unrepairable() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("DELETE FROM message_recipients WHERE message_id = 901")
                .unwrap();
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("recipient set is empty"), "{error}");
        });
    }

    #[test]
    fn source_statement_does_not_multiply_message_bodies_by_fanout() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync(
                    &format!("{SOURCE_SQL} LIMIT {}", MAX_RECIPIENTS + 2),
                    &[
                        901_i64.into(),
                        MAX_DB_PAYLOAD_BYTES.into(),
                        MAX_RECIPIENT_NAME_BYTES.into(),
                    ],
                )
                .unwrap();
            assert_eq!(rows.len(), 4);
            let bodies = rows
                .iter()
                .filter_map(|row| row.get_named::<Option<String>>("body_md").unwrap())
                .collect::<Vec<_>>();
            assert_eq!(bodies, vec!["Keep this body."]);
        });
    }

    #[test]
    fn source_lookup_preserves_cross_table_utf8_byte_boundary() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET body_md = 'Body λ🙂', subject = 'Résumé', thread_id = 'thread-π', topic = 'µ', archive_metadata_json = '{}' WHERE id = 901").unwrap();
            conn.execute_raw("UPDATE projects SET human_key = '/project/é' WHERE id = 101")
                .unwrap();
            // The joined project and sender bytes remain part of the same
            // total. UTF-8 characters are deliberately wider than one byte.
            let required: usize = [
                "Body λ🙂",
                "Résumé",
                "{}",
                "[]",
                "normal",
                "project",
                "/project/é",
                "OrangeHill",
                "thread-π",
                "µ",
                "{}",
            ]
            .into_iter()
            .map(str::len)
            .sum();
            let required = i64::try_from(required).unwrap();
            for limit in [required - 1, required, required + 1] {
                let params = [
                    901_i64.into(),
                    limit.into(),
                    MAX_RECIPIENT_NAME_BYTES.into(),
                ];
                let expected = conn
                    .query_sync(&source_sql_with_limit(LEGACY_SOURCE_SQL), &params)
                    .unwrap();
                let actual = conn
                    .query_sync(&source_sql_with_limit(SOURCE_SQL), &params)
                    .unwrap();
                assert_source_rows_equal(&expected, &actual);
                if limit < required {
                    assert_eq!(actual.len(), 3, "deliveries remain refusal witnesses");
                    assert!(prepare_source_rows(901, &actual).is_err());
                } else {
                    assert_eq!(actual.len(), 4);
                    let prepared = prepare_source_rows(901, &actual).unwrap();
                    assert_eq!(prepared.body, "Body λ🙂");
                    assert_eq!(prepared.message["project"], "/project/é");
                }
            }
            // Optional fields have zero cost when absent, including the
            // pre-migration archive metadata that still has a NULL default.
            conn.execute_raw("UPDATE messages SET thread_id = NULL, topic = NULL, archive_metadata_json = NULL WHERE id = 901").unwrap();
            let reduced = required - i64::try_from("thread-πµ{}".len()).unwrap();
            let params = [
                901_i64.into(),
                reduced.into(),
                MAX_RECIPIENT_NAME_BYTES.into(),
            ];
            let actual = conn
                .query_sync(&source_sql_with_limit(SOURCE_SQL), &params)
                .unwrap();
            assert_source_rows_equal(
                &conn
                    .query_sync(&source_sql_with_limit(LEGACY_SOURCE_SQL), &params)
                    .unwrap(),
                &actual,
            );
            assert!(prepare_source_rows(901, &actual).is_ok());
        });
    }

    #[test]
    fn source_lookup_keeps_one_snapshot_across_an_independent_commit() {
        fixture(|cx, pool, _| {
            let reader = outcome(block_on(pool.acquire(cx))).unwrap();
            let writer = DbConn::open_file(pool.sqlite_path()).unwrap();
            let params = [
                901_i64.into(),
                MAX_DB_PAYLOAD_BYTES.into(),
                MAX_RECIPIENT_NAME_BYTES.into(),
            ];
            let sql = source_sql_with_limit(SOURCE_SQL);
            reader.execute_raw("BEGIN").unwrap();
            let before = reader.query_sync(&sql, &params).unwrap();
            writer.execute_raw("BEGIN IMMEDIATE").unwrap();
            writer.execute_raw("UPDATE messages SET subject = 'Committed subject', body_md = 'Committed body' WHERE id = 901").unwrap();
            writer.execute_raw("UPDATE message_recipients SET read_ts = 17, ack_ts = 19 WHERE message_id = 901 AND agent_id = 102").unwrap();
            writer
                .execute_raw("UPDATE projects SET human_key = '/committed-project' WHERE id = 101")
                .unwrap();
            writer
                .execute_raw("UPDATE agents SET name = 'OrangeStone' WHERE id = 201")
                .unwrap();
            writer.execute_raw("COMMIT").unwrap();
            let pinned = reader.query_sync(&sql, &params).unwrap();
            assert_source_rows_equal(&before, &pinned);
            reader.execute_raw("COMMIT").unwrap();
            let current = reader.query_sync(&sql, &params).unwrap();
            assert_source_rows_equal(
                &reader
                    .query_sync(&source_sql_with_limit(LEGACY_SOURCE_SQL), &params)
                    .unwrap(),
                &current,
            );
            let prepared = prepare_source_rows(901, &current).unwrap();
            assert_eq!(prepared.body, "Committed body");
            assert_eq!(prepared.message["subject"], "Committed subject");
            assert_eq!(prepared.message["project"], "/committed-project");
            assert_eq!(prepared.sender, "OrangeStone");
            let receipt = current
                .iter()
                .find(|row| row.get_named::<Option<i64>>("recipient_id").unwrap() == Some(102))
                .unwrap();
            assert_eq!(receipt.get_named::<i64>("recipient_read_ts").unwrap(), 17);
            assert_eq!(receipt.get_named::<i64>("recipient_ack_ts").unwrap(), 19);
        });
    }

    fn seed_source_lookup_mailbox(conn: &DbConn) {
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(301, 'project-3', '/project-3', 1), (401, 'project-4', '/project-4', 1), (501, 'project-5', '/project-5', 1)").unwrap();
        let project_ids = [101, 201, 301, 401, 501];
        let agents = (0..76)
            .map(|index| {
                format!(
                    "({}, {}, 'LookupAgent{index}', 'test', 'test', '', 1, 1)",
                    1000 + index,
                    project_ids[index % 5],
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        conn.execute_raw(&format!("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES {agents}")).unwrap();
        let body = "Mailbox-scale body λ. ".repeat(48);
        for start in (0..8240).step_by(128) {
            let mut messages = Vec::new();
            let mut deliveries = Vec::new();
            let mut shared = Vec::new();
            for index in start..(start + 128).min(8240) {
                let id = 10_000 + index;
                let project = index % 5;
                let member = (index / 5) % 15;
                let sender = 1000 + member * 5 + project;
                messages.push(format!(
                    "({id}, {}, {sender}, 'Message {index}', '{body}', 'thread-{}', 'lookup', 'normal', 1, {}, '{{}}', '[]', '{{}}')",
                    project_ids[project],
                    index % 64,
                    1_000_000 + index,
                ));
                for (offset, kind) in [(1, "to"), (2, "cc"), (3, "bcc")] {
                    let recipient = 1000 + ((member + offset) % 15) * 5 + project;
                    deliveries.push(format!("({id}, {recipient}, '{kind}', 7, 11)"));
                }
                if index % 7 == 0 {
                    shared.push(format!("({id}, {}, 'to', 1)", project_ids[project]));
                }
            }
            conn.execute_raw(&format!("INSERT INTO messages(id, project_id, sender_id, subject, body_md, thread_id, topic, importance, ack_required, created_ts, recipients_json, attachments, archive_metadata_json) VALUES {}", messages.join(","))).unwrap();
            conn.execute_raw(&format!("INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) VALUES {}", deliveries.join(","))).unwrap();
            conn.execute_raw(&format!("INSERT INTO project_mailbox_deliveries(message_id, project_id, kind, delivered_ts) VALUES {}", shared.join(","))).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
    }

    /// Run alone with `--exact ... --nocapture --test-threads=1` to attribute
    /// the emitted process CPU ticks. Both statements execute against the same
    /// live native connection; elapsed time never substitutes for correctness.
    #[test]
    fn source_lookup_mailbox_scale_matches_legacy_in_one_query() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            seed_source_lookup_mailbox(&conn);
            for (table, expected) in [("projects", 5), ("agents", 82), ("messages", 8241)] {
                let count = conn
                    .query_sync(&format!("SELECT COUNT(*) AS n FROM {table}"), &[])
                    .unwrap()[0]
                    .get_named::<i64>("n")
                    .unwrap();
                assert_eq!(count, expected);
            }
            let sql = [
                source_sql_with_limit(LEGACY_SOURCE_SQL),
                source_sql_with_limit(SOURCE_SQL),
            ];
            let params = |id: i64| {
                [
                    id.into(),
                    MAX_DB_PAYLOAD_BYTES.into(),
                    MAX_RECIPIENT_NAME_BYTES.into(),
                ]
            };
            let plans = sql.each_ref().map(|sql| {
                conn.query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), &params(901))
                    .unwrap()
                    .into_iter()
                    .map(|row| format!("{:?}", row.get(3)))
                    .collect::<Vec<_>>()
            });
            for sql in &sql {
                conn.query_sync(sql, &params(901)).unwrap();
            }
            let mut wall_us = [Vec::new(), Vec::new()];
            let mut cpu_ticks = [Some(0_u64), Some(0_u64)];
            let mut queries = [0, 0];
            for sample in 0..8 {
                let id = [901, 10_000, 14_120, 18_239][sample % 4];
                let mut observations = [Vec::new(), Vec::new()];
                // Alternate order so only one candidate does not always
                // inherit the other candidate's warmed cache state.
                for variant in [sample % 2, 1 - sample % 2] {
                    let before_cpu = process_cpu_ticks();
                    let started = Instant::now();
                    observations[variant] = conn.query_sync(&sql[variant], &params(id)).unwrap();
                    wall_us[variant].push(started.elapsed().as_micros());
                    cpu_ticks[variant] = cpu_ticks[variant].and_then(|total| {
                        process_cpu_ticks()?
                            .checked_sub(before_cpu?)?
                            .checked_add(total)
                    });
                    queries[variant] += 1;
                }
                assert_source_rows_equal(&observations[0], &observations[1]);
                assert_eq!(
                    observations[1]
                        .iter()
                        .filter(|row| row
                            .get_named::<Option<String>>("body_md")
                            .unwrap()
                            .is_some())
                        .count(),
                    1,
                );
                let prepared = prepare_source_rows(id, &observations[1]).unwrap();
                assert_eq!(prepared.message["id"], id);
            }
            let mut production_queries = 0;
            let prepared = read_source(18_239, |sql, params| {
                production_queries += 1;
                conn.query_sync(sql, params)
                    .map_err(ReconcileFailure::source_unavailable)
            })
            .unwrap();
            assert_eq!(production_queries, 1);
            assert_eq!(prepared.message["id"], 18_239);
            assert_eq!(prepared.recipients.len(), 3);
            eprintln!(
                "GH354_MESSAGE_SOURCE {}",
                json!({
                    "messages": 8241, "agents": 82, "projects": 5,
                    "variants": ["legacy", "indexed"],
                    "query_plans": plans,
                    "measured_queries": queries,
                    "production_queries_per_observation": production_queries,
                    "wall_us": wall_us,
                    "process_cpu_ticks": cpu_ticks,
                }),
            );
        });
    }

    #[test]
    fn source_statement_selects_one_message_from_populated_delivery_tables() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("BEGIN IMMEDIATE").unwrap();
            // Enough unrelated deliveries to fill the observation's limit if
            // any UNION arm loses the message-ID binding. The foreign sender
            // of 901 is intentional and must still be accepted.
            for id in 1000..2200 {
                conn.execute_raw(&format!(
                    "INSERT INTO messages(id, project_id, sender_id, subject, body_md, created_ts) \
                     VALUES({id}, 201, 201, 'unrelated', 'not the selected body', 1); \
                     INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) \
                     VALUES({id}, 202, 'to', 19, 23); \
                     INSERT INTO project_mailbox_deliveries(message_id, project_id, kind, delivered_ts) \
                     VALUES({id}, 201, 'cc', 1)"
                ))
                .unwrap();
            }
            conn.execute_raw(
                "INSERT INTO project_mailbox_deliveries(message_id, project_id, kind, delivered_ts) \
                 VALUES(901, 101, 'cc', 1); COMMIT; PRAGMA query_only=ON",
            )
            .unwrap();
            let sql = format!("{SOURCE_SQL} LIMIT {}", MAX_RECIPIENTS + 2);
            let params = [
                901_i64.into(),
                MAX_DB_PAYLOAD_BYTES.into(),
                MAX_RECIPIENT_NAME_BYTES.into(),
            ];
            let rows = conn.query_sync(&sql, &params).unwrap();
            assert_eq!(rows.len(), 5);
            assert!(
                rows.iter()
                    .all(|row| row.get_named::<i64>("id").unwrap() == 901)
            );
            assert_eq!(
                rows.iter()
                    .map(|row| row.get_named::<i64>("row_kind").unwrap())
                    .collect::<Vec<_>>(),
                vec![0, 1, 1, 1, 2]
            );
            assert_eq!(rows[0].get_named::<i64>("sender_id").unwrap(), 201);
            assert_eq!(rows[0].get_named::<i64>("sender_project_id").unwrap(), 201);
            assert_eq!(rows[1].get_named::<i64>("recipient_read_ts").unwrap(), 7);
            assert_eq!(rows[1].get_named::<i64>("recipient_ack_ts").unwrap(), 11);
            assert_eq!(
                rows[4].get_named::<i64>("recipient_project_id").unwrap(),
                101
            );
            let prepared = prepare_source_rows(901, &rows).unwrap();
            assert_eq!(prepared.sender, "OrangeHill");
            assert_eq!(prepared.project_slug, "project");
            assert_eq!(prepared.body, "Keep this body.");
            assert_eq!(prepared.message["subject"], "Contact handoff");
            assert_eq!(prepared.message["to"], json!(["GreenStone"]));
            assert_eq!(
                prepared.message["cc"],
                json!(["GoldLeaf", "project:project"])
            );
            assert_eq!(prepared.message["bcc"], json!(["RedFox"]));
            assert_eq!(
                prepared.recipients,
                vec!["GoldLeaf", "GreenStone", "RedFox"]
            );
            assert_eq!(
                rows.iter()
                    .filter_map(|row| row.get_named::<Option<String>>("body_md").unwrap())
                    .collect::<Vec<_>>(),
                vec!["Keep this body."]
            );
            let mut absent = params;
            absent[0] = 999_i64.into();
            assert!(conn.query_sync(&sql, &absent).unwrap().is_empty());
        });
    }

    #[test]
    fn source_statement_preserves_byte_bounds_after_message_selection() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE messages SET body_md = 'λ', thread_id = 'thread-λ', topic = '部署', \
                 archive_metadata_json = '{}' WHERE id = 901; PRAGMA query_only=ON",
            )
            .unwrap();
            // Count UTF-8 bytes independently of SQL. A character-count cap
            // would wrongly admit the one-byte-over control below.
            let source_bytes: usize = [
                "λ",
                "Contact handoff",
                "{}",
                "[]",
                "normal",
                "project",
                "/project",
                "OrangeHill",
                "thread-λ",
                "部署",
                "{}",
            ]
            .into_iter()
            .map(str::len)
            .sum();
            let sql = format!("{SOURCE_SQL} LIMIT {}", MAX_RECIPIENTS + 2);
            let mut params = [
                901_i64.into(),
                i64::try_from(source_bytes).unwrap().into(),
                MAX_RECIPIENT_NAME_BYTES.into(),
            ];
            let rows = conn.query_sync(&sql, &params).unwrap();
            let exact = prepare_source_rows(901, &rows).unwrap();
            assert_eq!(exact.body, "λ");
            assert_eq!(exact.message["thread_id"], "thread-λ");
            assert_eq!(exact.message["topic"], "部署");
            assert!(exact.archive_metadata_known);

            params[1] = i64::try_from(source_bytes - 1).unwrap().into();
            let oversized = conn.query_sync(&sql, &params).unwrap();
            assert_eq!(oversized.len(), 3, "delivery witnesses remain visible");
            assert!(
                oversized
                    .iter()
                    .all(|row| row.get_named::<i64>("row_kind").unwrap() == 1)
            );
            assert!(
                prepare_source_rows(901, &oversized)
                    .err()
                    .unwrap()
                    .contains("oversized")
            );

            params[1] = i64::try_from(source_bytes).unwrap().into();
            params[2] = 9_i64.into(); // GreenStone is ten bytes.
            let oversized_name = conn.query_sync(&sql, &params).unwrap();
            assert_eq!(
                oversized_name.len(),
                4,
                "an invalid identity is not dropped by the join"
            );
            assert!(
                oversized_name[1]
                    .get_named::<Option<String>>("recipient_name")
                    .unwrap()
                    .is_none()
            );
            assert!(
                prepare_source_rows(901, &oversized_name)
                    .err()
                    .unwrap()
                    .contains("missing or oversized")
            );
        });
    }

    #[test]
    fn conflicting_populated_routing_never_publishes_or_changes_delivery_receipts() {
        fixture(|cx, pool, config| {
            for cached in [
                json!({"to": ["GreenStone"], "cc": ["GoldLeaf"], "bcc": []}),
                json!({"to": ["GreenStone", "RedFox"], "cc": ["GoldLeaf"], "bcc": []}),
                json!({"to": ["BlueLake"], "cc": ["GoldLeaf"], "bcc": ["RedFox"]}),
                json!({"to": ["GreenStone"], "cc": ["RedFox"], "bcc": ["GoldLeaf"]}),
                json!({"to": ["GreenStone", "GreenStone"], "cc": ["GoldLeaf"], "bcc": ["RedFox"]}),
            ] {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw(&format!(
                    "UPDATE messages SET recipients_json = '{cached}' WHERE id = 901"
                ))
                .unwrap();
                drop(conn);
                let error = prepare_message(cx, pool, 901).err().unwrap();
                assert!(
                    error.contains("conflicts with durable delivery rows"),
                    "{cached}: {error}"
                );
                let report = reconcile_message_batch(
                    cx,
                    pool,
                    config,
                    &mut ReconcileCursor::default(),
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!(report.scanned, 1);
                assert_eq!(report.deferred, 1);
                assert_eq!(report.repaired, 0);
                assert_eq!(report.files_created, 0);
                assert!(
                    !config
                        .storage_root
                        .join("projects/project/messages")
                        .exists()
                );
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                let rows = conn
                    .query_sync("SELECT recipients_json FROM messages WHERE id = 901", &[])
                    .unwrap();
                assert_eq!(
                    rows[0].get_named::<String>("recipients_json").unwrap(),
                    cached.to_string()
                );
                let rows = conn
                    .query_sync(
                        "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901 AND agent_id = 102",
                        &[],
                    )
                    .unwrap();
                assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 7);
                assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 11);
            }
        });
    }

    #[test]
    fn matching_populated_cache_preserves_order_and_existing_archive_bytes() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 101, 'to')",
            )
            .unwrap();
            conn.execute_raw("UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\",\"BlueLake\"],\"cc\":[\"GoldLeaf\"],\"bcc\":[\"RedFox\"]}' WHERE id = 901").unwrap();
            drop(conn);
            // Delivery rows are ordered by ID (BlueLake first); retain the
            // reverse order in the valid cache and in the surviving outbox.
            let message = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(message.message["to"], json!(["GreenStone", "BlueLake"]));
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &message.message,
                &message.sender,
                &message.recipients,
            )
            .unwrap()
            .0;
            let original =
                crate::render_message_bundle_content(&message.message, &message.body).unwrap();
            std::fs::create_dir_all(paths.outbox.parent().unwrap()).unwrap();
            std::fs::write(&paths.outbox, original.as_bytes()).unwrap();
            let report = reconcile_prepared(config, &message).unwrap();
            assert_eq!(report.files_created, 5);
            assert!(report.git_commit_needed);
            assert_eq!(std::fs::read(&paths.outbox).unwrap(), original.as_bytes());
            assert_eq!(
                std::fs::read(&paths.canonical).unwrap(),
                original.as_bytes()
            );
            assert_eq!(
                reconcile_prepared(config, &prepare_message(cx, pool, 901).unwrap()).unwrap(),
                ReconcileResult::default()
            );
        });
    }

    #[test]
    fn populated_cache_cannot_replace_missing_or_foreign_delivery_authority() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\"],\"cc\":[\"GoldLeaf\"],\"bcc\":[\"RedFox\"]}' WHERE id = 901").unwrap();
            conn.execute_raw("UPDATE message_recipients SET agent_id = 202 WHERE message_id = 901 AND agent_id = 102").unwrap();
            drop(conn);
            // The name still matches, but it names the foreign project's
            // GreenStone. A cache hit must not hide that identity mismatch.
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("different project"), "{error}");
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("DELETE FROM message_recipients WHERE message_id = 901")
                .unwrap();
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("recipient set is empty"), "{error}");
        });
    }

    #[test]
    fn oversized_delivery_identity_is_not_hidden_by_a_populated_cache() {
        fixture(|cx, pool, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\"],\"cc\":[\"GoldLeaf\"],\"bcc\":[\"RedFox\"]}' WHERE id = 901").unwrap();
            let name = "r".repeat(usize::try_from(MAX_RECIPIENT_NAME_BYTES).unwrap() + 1);
            conn.execute_raw(&format!("UPDATE agents SET name = '{name}' WHERE id = 103"))
                .unwrap();
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("missing or oversized"), "{error}");
        });
    }

    #[test]
    fn source_retains_an_overflow_witness_even_when_cached_routing_fits() {
        fixture(|cx, pool, _| {
            let additional = MAX_RECIPIENTS - 3;
            let agents = (0..additional)
                .map(|index| {
                    let id = 1000 + index;
                    format!("({id}, 101, 'Recipient{index}', 'test', 'test', '', 1, 1)")
                })
                .collect::<Vec<_>>()
                .join(",");
            let deliveries = (0..additional)
                .map(|index| format!("(901, {}, 'to')", 1000 + index))
                .collect::<Vec<_>>()
                .join(",");
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES{agents}"
            ))
            .unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES{deliveries}"
            ))
            .unwrap();
            drop(conn);
            let message = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(message.recipients.len(), MAX_RECIPIENTS);
            let cached = json!({
                "to": message.message["to"],
                "cc": message.message["cc"],
                "bcc": message.message["bcc"],
            });
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!(
                "UPDATE messages SET recipients_json = '{cached}' WHERE id = 901"
            ))
            .unwrap();
            // Two further deliveries make the SQL result truncate; it must
            // still include the first excess row rather than look complete.
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(9000, 101, 'OverflowRecipient', 'test', 'test', '', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 101, 'to'), (901, 9000, 'to')").unwrap();
            let rows = conn
                .query_sync(
                    &format!("{SOURCE_SQL} LIMIT {}", MAX_RECIPIENTS + 2),
                    &[
                        901_i64.into(),
                        MAX_DB_PAYLOAD_BYTES.into(),
                        MAX_RECIPIENT_NAME_BYTES.into(),
                    ],
                )
                .unwrap();
            assert_eq!(rows.len(), MAX_RECIPIENTS + 2);
            drop(conn);
            let error = prepare_message(cx, pool, 901).err().unwrap();
            assert!(error.contains("exceeds its bound"), "{error}");
        });
    }

    fn query_only(conn: &DbConn) -> i64 {
        conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
            .get_as::<i64>(0)
            .unwrap()
    }

    #[test]
    fn source_read_panics_retire_the_reader_and_leave_repairable_mail() {
        for after_projection in [false, true] {
            fixture(|cx, pool, config| {
                let failed = with_source_connection(
                    cx,
                    pool,
                    |conn| -> Result<PreparedMessage, ReconcileFailure> {
                        conn.execute_raw("PRAGMA query_only = ON").unwrap();
                        assert_eq!(query_only(conn), 1);
                        if after_projection {
                            let prepared = read_source(901, |sql, params| {
                                conn.query_sync(sql, params)
                                    .map_err(ReconcileFailure::source_unavailable)
                            })?;
                            assert_eq!(prepared.body, "Keep this body.");
                        }
                        panic!("private-source-panic-payload");
                    },
                );
                let failure = failed
                    .err()
                    .expect("a panic must not publish a source projection");
                assert!(
                    failure.transient,
                    "the bounded retry lane must retain this ID"
                );
                assert!(failure.detail.contains("panicked"));
                assert!(!failure.detail.contains("private-source-panic-payload"));
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert_eq!(query_only(&conn), 0, "panicked reader must not be reused");
                drop(conn);
                let report = reconcile_message_batch(
                    cx,
                    pool,
                    config,
                    &mut ReconcileCursor::default(),
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!(report.repaired, 1);
                assert_eq!(report.files_created, 5);
                assert_eq!(report.deferred, 0);
                let original = prepare_message(cx, pool, 901).unwrap();
                assert_eq!(original.body, "Keep this body.");
                assert_eq!(original.message["bcc"], json!(["RedFox"]));
                assert_eq!(
                    reconcile_prepared(config, &original).unwrap(),
                    ReconcileResult::default()
                );
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                let rows = conn
                    .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
                    .unwrap();
                assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 1);
                let receipts = conn.query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901 AND agent_id = 102",
                    &[],
                ).unwrap();
                assert_eq!(receipts[0].get_named::<i64>("read_ts").unwrap(), 7);
                assert_eq!(receipts[0].get_named::<i64>("ack_ts").unwrap(), 11);
            });
        }
    }

    #[test]
    fn failed_source_query_retires_reader_and_next_pass_repairs_without_redelivery() {
        fixture(|cx, pool, config| {
            let failure = with_source_connection(cx, pool, |conn| {
                // A connection-local witness distinguishes a replacement from
                // the same failed connection returned to this one-slot pool.
                conn.execute_raw("PRAGMA query_only = ON").unwrap();
                assert_eq!(query_only(conn), 1);
                conn.query_sync("SELECT * FROM missing_archive_source_probe", &[])
                    .map_err(ReconcileFailure::source_unavailable)
            })
            .unwrap_err();
            assert!(failure.transient);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(query_only(&conn), 0, "failed reader must not be reused");
            drop(conn);
            let report = reconcile_message_batch(
                cx,
                pool,
                config,
                &mut ReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 5);
            assert_eq!(report.deferred, 0);
            let original = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(original.body, "Keep this body.");
            assert_eq!(original.message["bcc"], json!(["RedFox"]));
            assert_eq!(
                reconcile_prepared(config, &original).unwrap(),
                ReconcileResult::default()
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 1);
            let receipts = conn.query_sync(
                "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901 AND agent_id = 102",
                &[],
            ).unwrap();
            assert_eq!(receipts[0].get_named::<i64>("read_ts").unwrap(), 7);
            assert_eq!(receipts[0].get_named::<i64>("ack_ts").unwrap(), 11);
        });
    }

    #[test]
    fn successful_source_read_keeps_healthy_pool_connection() {
        fixture(|cx, pool, _| {
            let original = with_source_connection(cx, pool, |conn| {
                conn.execute_raw("PRAGMA query_only = ON").unwrap();
                read_source(901, |sql, params| {
                    conn.query_sync(sql, params)
                        .map_err(ReconcileFailure::source_unavailable)
                })
            })
            .unwrap();
            assert_eq!(original.body, "Keep this body.");
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                query_only(&conn),
                1,
                "successful reads must retain the connection"
            );
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
        });
    }

    #[test]
    fn conflicting_source_metadata_does_not_retire_reader_or_weaken_refusal() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE messages SET recipients_json = '{\"to\":[\"GreenStone\"]}' WHERE id = 901",
            )
            .unwrap();
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            drop(conn);
            let failure = prepare_message_classified(cx, pool, 901).err().unwrap();
            assert!(
                !failure.transient,
                "conflicting evidence is not a failed reader"
            );
            assert!(failure.detail.contains("lacks cc array"));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                query_only(&conn),
                1,
                "metadata refusal must not churn the pool"
            );
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
            drop(conn);
            let report = reconcile_message_batch(
                cx,
                pool,
                config,
                &mut ReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.deferred_conflict, 1);
            assert_eq!(report.repaired, 0);
            assert_eq!(report.retry_len, 0);
            assert!(
                !config
                    .storage_root
                    .join("projects/project/messages")
                    .exists()
            );
        });
    }

    /// The reconciler renders the shared envelope key for key, in order; a
    /// reply appends `reply_to`. Tool-side writers are pinned to the same
    /// constant, so a repaired copy matches a queued write byte for byte.
    #[test]
    fn prepared_envelope_uses_the_shared_archive_key_order() {
        fixture(|cx, pool, _config| {
            let keys = |message: &Value| -> Vec<String> {
                message
                    .as_object()
                    .expect("envelope object")
                    .keys()
                    .cloned()
                    .collect()
            };
            let plain = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(keys(&plain.message), crate::MESSAGE_ARCHIVE_ENVELOPE_KEYS);

            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE messages SET thread_id = 'root', archive_metadata_json = '{\"reply_to\":7}' WHERE id = 901",
            )
            .unwrap();
            drop(conn);
            let reply = prepare_message(cx, pool, 901).unwrap();
            let mut expected: Vec<&str> = crate::MESSAGE_ARCHIVE_ENVELOPE_KEYS.to_vec();
            expected.push("reply_to");
            assert_eq!(keys(&reply.message), expected);
        });
    }
}
