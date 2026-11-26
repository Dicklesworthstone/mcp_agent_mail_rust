//! Preserve committed send retry receipts when a readable database is salvaged.
//!
//! Canonical message archives do not contain idempotency claims. Losing a live
//! claim while retaining its message would make a timeout retry send twice.
//! This lane runs inside reconstruction's existing merge transaction, after
//! project, agent, message and recipient identities have settled.

use crate::error::{DbError, DbResult};
use crate::models::MessageRow;
use crate::pool::SyncQuery;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sqlmodel_core::{Row, Value};
use std::collections::{HashMap, HashSet};

const ERROR_PREFIX: &str = "send idempotency recovery refused: ";
const METADATA_BATCH_ROWS: usize = 128;
// Public keys allow 255 Unicode characters (up to 1020 UTF-8 bytes). The DB
// accepts opaque fingerprints, so leave room beyond the usual 64 hex bytes.
const MAX_CLAIM_METADATA_BYTES: i64 = 4096;

pub(crate) struct SendClaimRecovery {
    has_live_claims: bool,
    cutoff: i64,
    source_has_topic: bool,
}

impl SendClaimRecovery {
    pub(crate) fn protect_merge<T>(&self, result: DbResult<T>) -> DbResult<T> {
        if self.has_live_claims {
            result.map_err(protect_error)
        } else {
            result
        }
    }
}

pub(crate) fn protect_error(error: DbError) -> DbError {
    if is_refusal(&error) {
        error
    } else {
        refusal("merge must retain committed send receipts", error)
    }
}

pub(crate) fn is_refusal(error: &DbError) -> bool {
    matches!(error, DbError::Internal(message) if message.starts_with(ERROR_PREFIX))
}

fn refusal(context: &str, detail: impl std::fmt::Display) -> DbError {
    DbError::Internal(format!(
        "{ERROR_PREFIX}{context}: {detail}; retry protection could not be preserved"
    ))
}

fn query(conn: &impl SyncQuery, sql: &str, params: &[Value]) -> DbResult<Vec<Row>> {
    conn.query_sync(sql, params)
        .map_err(|error| refusal("read required state", error))
}

fn one<'a>(rows: &'a [Row], context: &str) -> DbResult<&'a Row> {
    if rows.len() == 1 {
        Ok(&rows[0])
    } else {
        Err(refusal(context, "expected exactly one matching identity"))
    }
}

macro_rules! field {
    ($row:expr, $name:literal, $ty:ty) => {
        $row.get_named::<$ty>($name)
            .map_err(|error| refusal(concat!("decode ", $name), error))?
    };
}

fn real_table_exists(source: &impl SyncQuery, name: &str) -> DbResult<bool> {
    let rows = query(
        source,
        "SELECT type FROM sqlite_master WHERE name = ? COLLATE NOCASE",
        &[Value::Text(name.to_string())],
    )?;
    if rows.is_empty() {
        return Ok(false);
    }
    if field!(one(&rows, "recovery schema")?, "type", String) != "table" {
        return Err(refusal("recovery schema", "expected a real table"));
    }
    Ok(true)
}

fn migration_applied(source: &impl SyncQuery, id: &str) -> DbResult<bool> {
    if !real_table_exists(source, crate::schema::MIGRATIONS_TABLE_NAME)? {
        return Ok(false);
    }
    let rows = query(
        source,
        "SELECT 1 AS applied FROM mcp_agent_mail_migrations WHERE id = ? LIMIT 1",
        &[Value::Text(id.to_string())],
    )?;
    Ok(!rows.is_empty())
}

/// Establish retry authority before *any* merge work can fall back to an
/// archive-only candidate. The source has already passed integrity admission;
/// losing a table or failing this probe is not evidence of a legacy database.
pub(crate) fn inspect_source(source: &impl SyncQuery) -> DbResult<SendClaimRecovery> {
    let cutoff = crate::timestamps::now_micros();
    let mut recovery = SendClaimRecovery {
        has_live_claims: false,
        cutoff,
        source_has_topic: false,
    };
    if !real_table_exists(source, "idempotency_keys")? {
        if migration_applied(source, "v1_create_table_idempotency_keys")? {
            return Err(refusal(
                "idempotency schema",
                "migration ledger requires the missing idempotency_keys table",
            ));
        }
        return Ok(recovery);
    }

    // SQLite comparisons of text and integer timestamps do not establish
    // retention. Inspect storage types before selecting live claims.
    if !query(
        source,
        "SELECT 1 AS invalid FROM idempotency_keys \
         WHERE tool = 'send_message' AND typeof(expires_ts) <> 'integer' LIMIT 1",
        &[],
    )?
    .is_empty()
    {
        return Err(refusal("retention", "expiry is not an integer timestamp"));
    }
    recovery.has_live_claims = !query(
        source,
        "SELECT 1 AS live FROM idempotency_keys \
         WHERE tool = 'send_message' AND expires_ts >= ? LIMIT 1",
        &[Value::BigInt(cutoff)],
    )?
    .is_empty();
    if !recovery.has_live_claims {
        return Ok(recovery);
    }

    if !real_table_exists(source, "messages")? {
        return Err(refusal(
            "message schema",
            "live send receipts require the missing messages table",
        ));
    }
    let columns = query(source, "PRAGMA table_info(messages)", &[])?;
    for column in &columns {
        if field!(column, "name", String).eq_ignore_ascii_case("topic") {
            recovery.source_has_topic = true;
        }
    }
    if !recovery.source_has_topic && migration_applied(source, "v27_add_topic_to_messages")? {
        return Err(refusal(
            "message schema",
            "migration ledger requires the missing messages.topic column",
        ));
    }
    Ok(recovery)
}

fn mapped_id(map: &HashMap<i64, i64>, source_id: i64, context: &str) -> DbResult<i64> {
    map.get(&source_id)
        .copied()
        .filter(|id| source_id > 0 && *id > 0)
        .ok_or_else(|| {
            refusal(
                context,
                "recorded identity was not carried into the candidate",
            )
        })
}

fn verify_project(
    source: &impl SyncQuery,
    target: &impl SyncQuery,
    source_id: i64,
    target_id: i64,
) -> DbResult<()> {
    let sql = "SELECT slug, human_key FROM projects WHERE id = ?";
    let before = query(source, sql, &[Value::BigInt(source_id)])?;
    let after = query(target, sql, &[Value::BigInt(target_id)])?;
    let before = one(&before, "source project")?;
    let after = one(&after, "target project")?;
    let source_slug = field!(before, "slug", String);
    let target_slug = field!(after, "slug", String);
    let source_key = field!(before, "human_key", String);
    let target_key = field!(after, "human_key", String);
    // These are the same exact identities the salvage merger admits. A
    // basename, generated numeric id, or fuzzy path match is insufficient.
    if (!source_slug.trim().is_empty() && source_slug.trim() == target_slug.trim())
        || (!source_key.trim().is_empty() && source_key.trim() == target_key.trim())
    {
        Ok(())
    } else {
        Err(refusal(
            "project mapping",
            "stable project identity differs",
        ))
    }
}

fn verify_sender(
    source: &impl SyncQuery,
    target: &impl SyncQuery,
    source_sender: i64,
    target_sender: i64,
    source_scope: i64,
    target_scope: i64,
) -> DbResult<()> {
    let sql = "SELECT project_id, name FROM agents WHERE id = ?";
    let before = query(source, sql, &[Value::BigInt(source_sender)])?;
    let after = query(target, sql, &[Value::BigInt(target_sender)])?;
    let before = one(&before, "source sender")?;
    let after = one(&after, "target sender")?;
    let source_name = field!(before, "name", String);
    let target_name = field!(after, "name", String);
    if field!(before, "project_id", i64) != source_scope
        || field!(after, "project_id", i64) != target_scope
        || source_name.trim().is_empty()
        || !source_name.eq_ignore_ascii_case(&target_name)
    {
        return Err(refusal(
            "sender mapping",
            "sender identity does not belong to the claim's project",
        ));
    }
    Ok(())
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Recipients {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
}

fn recipients(raw: &str) -> DbResult<Recipients> {
    if !raw.trim_start().starts_with('{') {
        return Err(refusal("recipient result", "expected a recipient object"));
    }
    let mut result: Recipients =
        serde_json::from_str(raw).map_err(|error| refusal("recipient result", error))?;
    if result.to.is_empty() && result.cc.is_empty() && result.bcc.is_empty() {
        return Err(refusal("recipient result", "no recorded delivery"));
    }
    for names in [&mut result.to, &mut result.cc, &mut result.bcc] {
        for name in names.iter_mut() {
            if name.trim().is_empty() {
                return Err(refusal("recipient result", "empty recipient identity"));
            }
            // Agent identity is ASCII case-insensitive. Preserve the original
            // spelling in the receipt; normalize only this comparison.
            name.make_ascii_lowercase();
        }
        names.sort_unstable();
    }
    Ok(result)
}

fn attachments(raw: &str) -> DbResult<JsonValue> {
    let result: JsonValue =
        serde_json::from_str(raw).map_err(|error| refusal("attachment result", error))?;
    if !result.is_array() {
        return Err(refusal("attachment result", "expected an attachment array"));
    }
    Ok(result)
}

fn delivery_rows(conn: &impl SyncQuery, message_id: i64) -> DbResult<Vec<Row>> {
    let limit = crate::queries::MESSAGE_ARCHIVE_MAX_RECIPIENTS;
    let rows = query(
        conn,
        &format!(
            "SELECT mr.agent_id, a.project_id, \
             CASE WHEN length(CAST(a.name AS BLOB)) <= {} THEN a.name END AS name, \
             CASE WHEN length(CAST(mr.kind AS BLOB)) <= 3 THEN mr.kind END AS kind \
             FROM message_recipients mr LEFT JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? LIMIT {}",
            crate::queries::MESSAGE_ARCHIVE_MAX_RECIPIENT_NAME_BYTES,
            limit + 1
        ),
        &[Value::BigInt(message_id)],
    )?;
    if rows.len() > limit {
        return Err(refusal(
            "delivery mapping",
            "exceeds archive recipient limit",
        ));
    }
    Ok(rows)
}

fn shared_delivery(conn: &impl SyncQuery, message_id: i64) -> DbResult<Vec<Row>> {
    if !real_table_exists(conn, "project_mailbox_deliveries")? {
        return Ok(Vec::new());
    }
    query(
        conn,
        "SELECT project_id, \
         CASE WHEN length(CAST(kind AS BLOB)) <= 3 THEN kind END AS kind \
         FROM project_mailbox_deliveries WHERE message_id = ? LIMIT 2",
        &[Value::BigInt(message_id)],
    )
}

#[allow(clippy::too_many_lines)]
fn verify_deliveries(
    source: &impl SyncQuery,
    target: &impl SyncQuery,
    source_message: i64,
    target_message: i64,
    project_ids: &HashMap<i64, i64>,
    agent_ids: &HashMap<i64, i64>,
) -> DbResult<()> {
    let before = delivery_rows(source, source_message)?;
    let after = delivery_rows(target, target_message)?;
    if before.len() != after.len() {
        return Err(refusal("delivery mapping", "recipient count differs"));
    }
    let mut target_by_id = HashMap::new();
    for row in &after {
        target_by_id.insert(field!(row, "agent_id", i64), row);
    }
    let mut mapped = HashSet::new();
    for row in &before {
        let source_agent = field!(row, "agent_id", i64);
        let target_agent = mapped_id(agent_ids, source_agent, "recipient agent")?;
        if !mapped.insert(target_agent) {
            return Err(refusal("delivery mapping", "distinct recipients collapsed"));
        }
        let target_row = target_by_id
            .get(&target_agent)
            .ok_or_else(|| refusal("delivery mapping", "mapped recipient is missing"))?;
        let source_project = field!(row, "project_id", i64);
        let target_project = mapped_id(project_ids, source_project, "recipient project")?;
        let source_name = field!(row, "name", String);
        let target_name = field!(target_row, "name", String);
        let kind = field!(row, "kind", String);
        if field!(target_row, "project_id", i64) != target_project
            || source_name.trim().is_empty()
            || !source_name.eq_ignore_ascii_case(&target_name)
            || !matches!(kind.as_str(), "to" | "cc" | "bcc")
            || field!(target_row, "kind", String) != kind
        {
            return Err(refusal(
                "delivery mapping",
                "recipient identity or role differs",
            ));
        }
        verify_project(source, target, source_project, target_project)?;
    }

    // A project address is one shared delivery, not a recipient row per agent.
    // Keep it independent so project-mailbox-only sends remain recoverable.
    let source_shared = shared_delivery(source, source_message)?;
    let target_shared = shared_delivery(target, target_message)?;
    if source_shared.is_empty() && target_shared.is_empty() {
        if before.is_empty() {
            return Err(refusal("delivery mapping", "no durable delivery exists"));
        }
        return Ok(());
    }
    let source_shared = one(&source_shared, "source shared delivery")?;
    let target_shared = one(&target_shared, "target shared delivery")?;
    let source_project = field!(source_shared, "project_id", i64);
    let target_project = mapped_id(project_ids, source_project, "shared delivery project")?;
    let kind = field!(source_shared, "kind", String);
    if field!(target_shared, "project_id", i64) != target_project
        || !matches!(kind.as_str(), "to" | "cc")
        || field!(target_shared, "kind", String) != kind
    {
        return Err(refusal(
            "delivery mapping",
            "shared project or role differs",
        ));
    }
    verify_project(source, target, source_project, target_project)
}

fn verify_message(
    conn: &impl SyncQuery,
    expected: &MessageRow,
    reconstructed: bool,
    has_topic: bool,
) -> DbResult<()> {
    let id = expected
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| refusal("message result", "missing positive message id"))?;
    // Full payloads are read one at a time and share the archive reader's cap.
    // A damaged target row must not defeat that bound during identity checks.
    let topic = if has_topic { "topic" } else { "NULL" };
    let rows = query(
        conn,
        &format!(
            "SELECT project_id, sender_id, {topic} AS topic, subject, body_md, importance, \
         ack_required, created_ts, recipients_json, attachments FROM messages \
         WHERE id = ? AND \
         length(CAST(subject AS BLOB)) + length(CAST(body_md AS BLOB)) + \
         COALESCE(length(CAST({topic} AS BLOB)), 0) + length(CAST(importance AS BLOB)) + \
         length(CAST(attachments AS BLOB)) + length(CAST(recipients_json AS BLOB)) <= ?"
        ),
        &[
            Value::BigInt(id),
            Value::BigInt(
                i64::try_from(crate::reconstruct::MAX_ARCHIVE_ARTIFACT_BYTES).unwrap_or(i64::MAX),
            ),
        ],
    )?;
    let row = one(
        &rows,
        "message missing, ambiguous, or beyond archive size limit",
    )?;
    let body = field!(row, "body_md", String);
    // render_message_bundle_content appends the body without a trailing
    // newline; extract_body_after_frontmatter removes only leading CR/LF.
    // Allow precisely that existing archive projection in the target. The
    // accepted result must match the source body byte-for-byte.
    let body_matches = body == expected.body_md
        || (reconstructed && body == expected.body_md.trim_start_matches(['\n', '\r']));
    if field!(row, "project_id", i64) != expected.project_id
        || field!(row, "sender_id", i64) != expected.sender_id
        || field!(row, "created_ts", i64) != expected.created_ts
        || field!(row, "subject", String) != expected.subject
        || field!(row, "topic", Option<String>) != expected.topic
        || field!(row, "importance", String) != expected.importance
        || field!(row, "ack_required", i64) != expected.ack_required
        || !body_matches
        || recipients(&field!(row, "recipients_json", String))?
            != recipients(&expected.recipients_json)?
        || attachments(&field!(row, "attachments", String))? != attachments(&expected.attachments)?
    {
        return Err(refusal(
            "message identity",
            "recorded immutable content or delivery differs",
        ));
    }
    // thread_id is mutable: a TUI reassignment must not invalidate a frozen
    // send receipt or replace its original response with the current thread.
    Ok(())
}

/// Restore unexpired `send_message` claims inside the caller's merge transaction.
///
/// Other tools have different recorded-result and request-identity contracts.
/// In particular, reply fingerprints contain a numeric parent id; do not
/// reinterpret those fingerprints as send receipts during an id remap.
#[allow(clippy::too_many_lines)]
pub(crate) fn salvage_send_claims(
    source: &impl SyncQuery,
    target: &crate::CanonicalDbConn,
    project_ids: &HashMap<i64, i64>,
    agent_ids: &HashMap<i64, i64>,
    message_ids: &HashMap<i64, i64>,
    recovery: &SendClaimRecovery,
) -> DbResult<()> {
    if !recovery.has_live_claims {
        return Ok(());
    }

    let mut after: Option<(i64, String)> = None;
    loop {
        let mut params = Vec::new();
        let continuation = if let Some((project, key)) = &after {
            params.extend([
                Value::BigInt(*project),
                Value::BigInt(*project),
                Value::Text(key.clone()),
            ]);
            " AND (idempotency_keys.project_id > ? OR \
                   (idempotency_keys.project_id = ? AND idempotency_keys.idempotency_key > ?))"
        } else {
            ""
        };
        // inspect_source checked the storage type of every send expiry before
        // this filter. Expired orphaned claims need no identity mapping.
        // Payloads contain entire messages; page only metadata and fetch one
        // bounded result separately. CASE bounds even corrupt key/fingerprint
        // strings before the driver can materialize them into a metadata page.
        params.insert(0, Value::BigInt(recovery.cutoff));
        let rows = query(
            source,
            &format!(
                "SELECT project_id, \
                 CASE WHEN length(CAST(idempotency_key AS BLOB)) <= {MAX_CLAIM_METADATA_BYTES} \
                      THEN idempotency_key END AS idempotency_key, \
                 CASE WHEN length(CAST(payload_fingerprint AS BLOB)) <= {MAX_CLAIM_METADATA_BYTES} \
                      THEN payload_fingerprint END AS payload_fingerprint, \
                 length(CAST(idempotency_key AS BLOB)) AS key_bytes, \
                 length(CAST(payload_fingerprint AS BLOB)) AS fingerprint_bytes, \
                 created_ts, expires_ts, \
                 typeof(expires_ts) AS expires_type, typeof(created_ts) AS created_type, \
                 length(CAST(result_json AS BLOB)) AS result_bytes \
                 FROM idempotency_keys WHERE tool = 'send_message' AND expires_ts >= ?{continuation} \
                 ORDER BY idempotency_keys.project_id, idempotency_keys.idempotency_key \
                 LIMIT {METADATA_BATCH_ROWS}"
            ),
            &params,
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            if !(1..=MAX_CLAIM_METADATA_BYTES).contains(&field!(row, "key_bytes", i64))
                || !(1..=MAX_CLAIM_METADATA_BYTES).contains(&field!(row, "fingerprint_bytes", i64))
            {
                return Err(refusal(
                    "claim metadata",
                    "empty or oversized key/fingerprint",
                ));
            }
            let source_scope = field!(row, "project_id", i64);
            let key = field!(row, "idempotency_key", String);
            after = Some((source_scope, key.clone()));
            if field!(row, "expires_type", String) != "integer" {
                return Err(refusal("retention", "expiry is not an integer timestamp"));
            }
            let expires = field!(row, "expires_ts", i64);
            if expires < recovery.cutoff {
                return Err(refusal("retention", "source changed during salvage"));
            }
            let created = field!(row, "created_ts", i64);
            let fingerprint = field!(row, "payload_fingerprint", String);
            let result_bytes = field!(row, "result_bytes", i64);
            if field!(row, "created_type", String) != "integer"
                || created > expires
                || key.trim().is_empty()
                || fingerprint.is_empty()
            {
                return Err(refusal("claim metadata", "invalid live claim"));
            }
            if result_bytes < 0
                || u64::try_from(result_bytes).unwrap_or(u64::MAX)
                    > crate::reconstruct::MAX_ARCHIVE_ARTIFACT_BYTES
            {
                return Err(refusal("recorded result", "exceeds archive size limit"));
            }

            let original = query(
                source,
                "SELECT result_json FROM idempotency_keys \
                 WHERE project_id = ? AND tool = 'send_message' AND idempotency_key = ? \
                 AND length(CAST(result_json AS BLOB)) <= ?",
                &[
                    Value::BigInt(source_scope),
                    Value::Text(key.clone()),
                    Value::BigInt(
                        i64::try_from(crate::reconstruct::MAX_ARCHIVE_ARTIFACT_BYTES)
                            .unwrap_or(i64::MAX),
                    ),
                ],
            )?;
            let raw = field!(one(&original, "recorded result")?, "result_json", String);
            // Deserialize the typed row directly so duplicate known identity
            // fields are rejected before building the editable JSON object.
            let mut receipt: MessageRow =
                serde_json::from_str(&raw).map_err(|error| refusal("recorded result", error))?;
            verify_message(source, &receipt, false, recovery.source_has_topic)?;

            let target_scope = mapped_id(project_ids, source_scope, "claim project")?;
            let target_project = mapped_id(project_ids, receipt.project_id, "delivery project")?;
            let target_sender = mapped_id(agent_ids, receipt.sender_id, "sender")?;
            let target_message = mapped_id(message_ids, receipt.id.unwrap_or(0), "message")?;
            verify_project(source, target, source_scope, target_scope)?;
            verify_project(source, target, receipt.project_id, target_project)?;
            // Cross-project sends are scoped to the sender's project while
            // their MessageRow belongs to the recipient's delivery project.
            verify_sender(
                source,
                target,
                receipt.sender_id,
                target_sender,
                source_scope,
                target_scope,
            )?;
            verify_deliveries(
                source,
                target,
                receipt.id.unwrap_or(0),
                target_message,
                project_ids,
                agent_ids,
            )?;
            receipt.id = Some(target_message);
            receipt.project_id = target_project;
            receipt.sender_id = target_sender;
            verify_message(target, &receipt, true, true)?;

            // Preserve the original payload, including thread_id, list order,
            // nested JSON strings and any future receipt fields. Only the
            // three database identity fields need translation.
            let mut result: JsonValue =
                serde_json::from_str(&raw).map_err(|error| refusal("recorded result", error))?;
            let object = result
                .as_object_mut()
                .ok_or_else(|| refusal("recorded result", "expected a JSON object"))?;
            object.insert("id".to_string(), JsonValue::from(target_message));
            object.insert("project_id".to_string(), JsonValue::from(target_project));
            object.insert("sender_id".to_string(), JsonValue::from(target_sender));
            let result = serde_json::to_string(&result)
                .map_err(|error| refusal("encode mapped result", error))?;
            // Never ignore a key collision introduced by coalesced projects.
            // The entire caller-owned salvage transaction rolls back on error.
            target
                .execute_sync(
                    "INSERT INTO idempotency_keys \
                     (project_id, tool, idempotency_key, payload_fingerprint, result_json, created_ts, expires_ts) \
                     VALUES (?, 'send_message', ?, ?, ?, ?, ?)",
                    &[
                        Value::BigInt(target_scope),
                        Value::Text(key),
                        Value::Text(fingerprint),
                        Value::Text(result),
                        Value::BigInt(created),
                        Value::BigInt(expires),
                    ],
                )
                .map_err(|error| refusal("insert mapped claim", error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalDbConn;

    fn mailbox(offset: i64, has_topic: bool) -> (CanonicalDbConn, MessageRow) {
        let conn = CanonicalDbConn::open_memory().unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        if !has_topic {
            conn.execute_raw(
                "DROP INDEX idx_messages_project_topic; ALTER TABLE messages DROP COLUMN topic;",
            )
            .unwrap();
        }
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, 'mailbox', '/mailbox', 1)",
            &[Value::BigInt(offset + 1)],
        )
        .unwrap();
        for (id, name) in [(2, "RedFox"), (3, "BlueLake")] {
            conn.execute_sync(
                "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) \
                 VALUES (?, ?, ?, 'test', 'test', 1, 1)",
                &[
                    Value::BigInt(offset + id),
                    Value::BigInt(offset + 1),
                    Value::Text(name.to_string()),
                ],
            )
            .unwrap();
        }
        let message = MessageRow {
            id: Some(offset + 4),
            project_id: offset + 1,
            sender_id: offset + 2,
            thread_id: Some("original-thread".to_string()),
            topic: None,
            subject: "original subject".to_string(),
            body_md: "\r\noriginal body\n".to_string(),
            importance: "normal".to_string(),
            ack_required: 1,
            created_ts: 42,
            recipients_json: r#"{"to":["BlueLake"],"cc":[],"bcc":[]}"#.to_string(),
            attachments: r#"[{"type":"image","path":"attachments/example.png"}]"#.to_string(),
        };
        conn.execute_sync(
            "INSERT INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, 'current-thread', ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(offset + 4),
                Value::BigInt(message.project_id),
                Value::BigInt(message.sender_id),
                Value::Text(message.subject.clone()),
                Value::Text(message.body_md.clone()),
                Value::Text(message.importance.clone()),
                Value::BigInt(message.ack_required),
                Value::BigInt(message.created_ts),
                Value::Text(message.recipients_json.clone()),
                Value::Text(message.attachments.clone()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, 'to')",
            &[Value::BigInt(offset + 4), Value::BigInt(offset + 3)],
        )
        .unwrap();
        (conn, message)
    }

    fn record(conn: &CanonicalDbConn, message: &MessageRow, key: &str, expires: i64) {
        conn.execute_sync(
            "INSERT INTO idempotency_keys \
             (project_id, tool, idempotency_key, payload_fingerprint, result_json, created_ts, expires_ts) \
             VALUES (?, 'send_message', ?, 'opaque-fingerprint', ?, 1, ?)",
            &[
                Value::BigInt(message.project_id),
                Value::Text(key.to_string()),
                Value::Text(serde_json::to_string(message).unwrap()),
                Value::BigInt(expires),
            ],
        )
        .unwrap();
    }

    fn restore(source: &CanonicalDbConn, target: &CanonicalDbConn) -> DbResult<()> {
        let recovery = inspect_source(source)?;
        salvage_send_claims(
            source,
            target,
            &HashMap::from([(101, 1)]),
            &HashMap::from([(102, 2), (103, 3)]),
            &HashMap::from([(104, 4)]),
            &recovery,
        )
    }

    #[test]
    fn missing_key_table_is_legacy_only_without_migration_authority() {
        let source = CanonicalDbConn::open_memory().unwrap();
        assert!(!inspect_source(&source).unwrap().has_live_claims);
        source
            .execute_raw(
                "CREATE TABLE mcp_agent_mail_migrations (id TEXT PRIMARY KEY); \
             INSERT INTO mcp_agent_mail_migrations VALUES ('v1_create_table_idempotency_keys');",
            )
            .unwrap();
        let error = inspect_source(&source)
            .err()
            .expect("missing authoritative table");
        assert!(is_refusal(&error));
        assert!(error.to_string().contains("migration ledger"));
    }

    #[test]
    fn legacy_topic_and_exact_archive_projection_preserve_frozen_response() {
        let (source, original) = mailbox(100, false);
        let (target, _) = mailbox(0, true);
        record(&source, &original, "legacy-topic", i64::MAX);
        target
            .execute_raw(
                "UPDATE messages SET body_md = 'original body' || char(10), \
             attachments = '[{\"path\":\"attachments/example.png\",\"type\":\"image\"}]'",
            )
            .unwrap();
        restore(&source, &target).unwrap();
        let result = target
            .query_sync("SELECT result_json FROM idempotency_keys", &[])
            .unwrap();
        let result: JsonValue =
            serde_json::from_str(&result[0].get_as::<String>(0).unwrap()).unwrap();
        let mut expected = serde_json::to_value(&original).unwrap();
        expected["id"] = JsonValue::from(4);
        expected["project_id"] = JsonValue::from(1);
        expected["sender_id"] = JsonValue::from(2);
        assert_eq!(result, expected);

        source
            .execute_raw(
                "CREATE TABLE mcp_agent_mail_migrations (id TEXT PRIMARY KEY); \
             INSERT INTO mcp_agent_mail_migrations VALUES ('v27_add_topic_to_messages');",
            )
            .unwrap();
        assert!(is_refusal(
            &inspect_source(&source).err().expect("missing modern topic")
        ));
    }

    #[test]
    fn altered_body_and_same_name_wrong_recipient_project_refuse_replay() {
        let (source, original) = mailbox(100, true);
        let (target, _) = mailbox(0, true);
        record(&source, &original, "identity", i64::MAX);
        target
            .execute_raw("UPDATE messages SET body_md = 'original body'")
            .unwrap();
        let error = restore(&source, &target).unwrap_err();
        assert!(error.to_string().contains("immutable content"));
        target
            .execute_sync(
                "UPDATE messages SET body_md = ?",
                &[Value::Text(original.body_md.clone())],
            )
            .unwrap();
        target.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (9, 'other', '/other', 1); \
             UPDATE agents SET project_id = 9 WHERE name = 'BlueLake';",
        ).unwrap();
        let error = restore(&source, &target).unwrap_err();
        assert!(error.to_string().contains("recipient identity"));
        assert!(
            target
                .query_sync("SELECT 1 FROM idempotency_keys", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_expiry_and_oversized_metadata_refuse_instead_of_disappearing() {
        let (source, original) = mailbox(100, true);
        let (target, _) = mailbox(0, true);
        record(&source, &original, "malformed", i64::MAX);
        source
            .execute_raw("UPDATE idempotency_keys SET expires_ts = 'not-a-timestamp'")
            .unwrap();
        assert!(is_refusal(
            &inspect_source(&source).err().expect("invalid expiry")
        ));
        source
            .execute_sync(
                "UPDATE idempotency_keys SET expires_ts = ?, payload_fingerprint = ?",
                &[Value::BigInt(i64::MAX), Value::Text("x".repeat(4097))],
            )
            .unwrap();
        let error = restore(&source, &target).unwrap_err();
        assert!(error.to_string().contains("oversized key/fingerprint"));
    }

    #[test]
    fn project_mailbox_only_receipt_keeps_shared_delivery_identity() {
        let (source, mut original) = mailbox(100, true);
        let (target, _) = mailbox(0, true);
        original.recipients_json = r#"{"to":["project:mailbox"],"cc":[],"bcc":[]}"#.to_string();
        for (conn, offset) in [(&source, 100), (&target, 0)] {
            conn.execute_raw("DELETE FROM message_recipients").unwrap();
            conn.execute_sync(
                "UPDATE messages SET recipients_json = ?",
                &[Value::Text(original.recipients_json.clone())],
            )
            .unwrap();
            conn.execute_sync(
                "INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts) \
                 VALUES (?, ?, 'to', 42)",
                &[Value::BigInt(offset + 4), Value::BigInt(offset + 1)],
            ).unwrap();
        }
        record(&source, &original, "shared", i64::MAX);
        restore(&source, &target).unwrap();
        assert_eq!(
            target
                .query_sync("SELECT 1 FROM idempotency_keys", &[])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn metadata_pagination_keeps_unicode_keys_and_expiry_boundary() {
        let (source, original) = mailbox(100, true);
        let (target, _) = mailbox(0, true);
        for number in 0..=METADATA_BATCH_ROWS {
            record(&source, &original, &format!("key-{number:03}"), 7);
        }
        record(&source, &original, &"🔑".repeat(255), 7);
        let recovery = SendClaimRecovery {
            has_live_claims: true,
            cutoff: 7,
            source_has_topic: true,
        };
        salvage_send_claims(
            &source,
            &target,
            &HashMap::from([(101, 1)]),
            &HashMap::from([(102, 2), (103, 3)]),
            &HashMap::from([(104, 4)]),
            &recovery,
        )
        .unwrap();
        let rows = target
            .query_sync(
                "SELECT COUNT(*) AS count, MIN(expires_ts) AS expiry FROM idempotency_keys",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows[0].get_named::<i64>("count").unwrap(),
            i64::try_from(METADATA_BATCH_ROWS + 2).unwrap()
        );
        assert_eq!(rows[0].get_named::<i64>("expiry").unwrap(), 7);
    }
}
