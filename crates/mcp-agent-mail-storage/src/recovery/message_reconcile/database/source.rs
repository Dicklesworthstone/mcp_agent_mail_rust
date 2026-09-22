//! One bounded mailbox observation for archive recovery, including legacy mail.
//!
//! The recipient cache is a rendering optimization, not the only durable copy
//! of a delivery. Legacy rows may still have its original `{}` default. Keep
//! message metadata and the recipient rows in one SQL statement, without
//! multiplying a large message body by its number of recipients.

use std::collections::HashSet;

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_db::sqlmodel_core::{Row, Value as SqlValue};
use serde_json::{Value, json};

use super::super::MAX_RECIPIENTS;
use super::{MAX_DB_PAYLOAD_BYTES, PreparedMessage, outcome, source_error};

const MAX_RECIPIENT_NAME_BYTES: i64 = 1024;

// UNION ALL gives the message one row and each delivery one small row. Both
// arms belong to the same statement/snapshot. LEFT JOIN is intentional: a
// missing recipient identity must be reported, not silently dropped by a join.
// The extra row beyond the recipient limit is an overflow witness, not mail
// that the recovery worker is allowed to discard.
const SOURCE_SQL: &str = "\
SELECT 0 AS row_kind, m.id, m.project_id, m.subject, m.body_md, m.thread_id, m.topic, \
       m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
       p.slug AS project_slug, p.human_key AS project_key, a.name AS sender, \
       NULL AS recipient_id, NULL AS recipient_project_id, \
       NULL AS recipient_name, NULL AS recipient_kind \
FROM messages m JOIN projects p ON p.id = m.project_id \
JOIN agents a ON a.id = m.sender_id \
WHERE m.id = ?1 AND \
      length(CAST(m.body_md AS BLOB)) + length(CAST(m.subject AS BLOB)) + \
      length(CAST(m.recipients_json AS BLOB)) + length(CAST(m.attachments AS BLOB)) + \
      length(CAST(m.importance AS BLOB)) + length(CAST(p.slug AS BLOB)) + \
      length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
      COALESCE(length(CAST(m.thread_id AS BLOB)), 0) + \
      COALESCE(length(CAST(m.topic AS BLOB)), 0) <= ?2 \
UNION ALL \
SELECT 1, mr.message_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
       NULL, NULL, NULL, mr.agent_id, a.project_id, \
       CASE WHEN length(CAST(a.name AS BLOB)) <= ?3 THEN a.name ELSE NULL END, \
       CASE WHEN length(CAST(mr.kind AS BLOB)) <= 3 THEN mr.kind ELSE NULL END \
FROM message_recipients mr LEFT JOIN agents a ON a.id = mr.agent_id \
WHERE mr.message_id = ?1 \
ORDER BY row_kind, recipient_id";

pub(super) fn prepare_message(cx: &Cx, pool: &DbPool, id: i64) -> Result<PreparedMessage, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    read_source(id, |sql, params| {
        conn.query_sync(sql, params).map_err(source_error)
    })
}

fn read_source(
    id: i64,
    query: impl FnOnce(&str, &[SqlValue]) -> Result<Vec<Row>, String>,
) -> Result<PreparedMessage, String> {
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
    Ok(PreparedMessage {
        message,
        body,
        sender,
        project_slug,
        recipients,
        payload_bytes,
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

fn routing_names(routing: &Value) -> Result<Vec<String>, String> {
    let mut recipients = Vec::new();
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
            recipients.push(name.to_string());
            if recipients.len() > MAX_RECIPIENTS {
                return Err("message recipient budget exceeded".to_string());
            }
        }
    }
    if recipients.is_empty() {
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
        if integer("row_kind")? != 1 || integer("id")? != id {
            return Err("recipient projection has a mismatched message identity".to_string());
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
}
