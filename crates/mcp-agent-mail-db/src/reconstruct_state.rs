//! Durable coordination state for archive-only reconstruction (GH#404).
//!
//! Receipts and contact edges are replayed by stable identities. Integer inbox
//! cursors cannot describe a new delivery ledger: retain a durable ceiling for
//! every cursor issued to a client, and explicitly expire the old generation
//! when only the archive survives. A readable salvage database still supplies
//! the original ledger through `reconstruct_delivery`.

use crate::CanonicalDbConn;
use crate::error::{DbError, DbResult};
use crate::pool::SyncQuery;
use crate::recovery_state::{
    AgentIdentity, ContactRecord, ProjectIdentity, ReceiptRecord, RecoveryRecord,
};
use fs2::FileExt as _;
use mcp_agent_mail_core::journal_io::{JournalDirectory, JournalFileMode};
use serde::{Deserialize, Serialize};
use sqlmodel_core::Value;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

const CURSOR_DIRECTORY: &str = "recovery-state";
const CURSOR_FILE: &str = "delivery-cursor.json";
const CURSOR_LOCK: &str = "delivery-cursor.lock";
const MAX_CURSOR_BYTES: u64 = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorCeiling {
    schema_version: u32,
    issued_through: i64,
}

fn error(detail: impl std::fmt::Display) -> DbError {
    DbError::Sqlite(format!("archive recovery state: {detail}"))
}

fn read_ceiling(directory: &JournalDirectory) -> DbResult<Option<i64>> {
    let file = match directory.open_file(CURSOR_FILE, JournalFileMode::Read) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(error(cause)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CURSOR_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() as u64 > MAX_CURSOR_BYTES {
        return Err(error("delivery cursor ceiling exceeds its size limit"));
    }
    let ceiling: CursorCeiling = serde_json::from_slice(&bytes).map_err(error)?;
    if ceiling.schema_version != 1 || ceiling.issued_through < 0 {
        return Err(error(
            "invalid delivery cursor ceiling or unsupported version",
        ));
    }
    Ok(Some(ceiling.issued_through))
}

/// Persist the cursor bound before a successful API/CLI response can expose it.
/// The global ceiling is monotonic across database generations and concurrent
/// readers. A failed write is an error, never an unprotected successful read.
pub fn record_issued_cursor(storage_root: &Path, cursor: i64) -> DbResult<()> {
    if cursor < 0 {
        return Err(error("cannot archive a negative delivery cursor"));
    }
    if cursor == 0 {
        return Ok(());
    }
    let directory = JournalDirectory::open(storage_root, CURSOR_DIRECTORY, true).map_err(error)?;
    let lock = directory
        .open_file(CURSOR_LOCK, JournalFileMode::Lock)
        .map_err(error)?;
    let started = Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(cause)
                if cause.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(2) =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(cause) => return Err(error(cause)),
        }
    }
    directory.validate_file(CURSOR_LOCK, &lock).map_err(error)?;
    if read_ceiling(&directory)?.is_some_and(|ceiling| ceiling >= cursor) {
        return Ok(());
    }
    let ceiling = CursorCeiling {
        schema_version: 1,
        issued_through: cursor,
    };
    directory
        .atomic_write(CURSOR_FILE, |file| {
            serde_json::to_writer(&mut *file, &ceiling).map_err(std::io::Error::other)?;
            file.write_all(b"\n")
        })
        .map_err(error)?;
    directory.validate_file(CURSOR_LOCK, &lock).map_err(error)?;
    crate::recovery_state::note_pending_archive_paths(
        storage_root,
        [format!("{CURSOR_DIRECTORY}/{CURSOR_FILE}")],
    );
    Ok(())
}

/// Protect cursors issued by an older version before this version first serves
/// the healthy database. Include pruned tails and the AUTOINCREMENT allocator.
pub fn checkpoint_cursor(conn: &impl SyncQuery, storage_root: &Path) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT MAX(value) AS ceiling FROM (\
             SELECT COALESCE(MAX(seq), 0) AS value FROM sqlite_sequence WHERE name = 'inbox_delivery_events' \
             UNION ALL SELECT COALESCE(MAX(tail_seq), 0) FROM inbox_delivery_cursor_state \
             UNION ALL SELECT COALESCE(MAX(seq), 0) FROM inbox_delivery_events)",
            &[],
        )
        .map_err(error)?;
    let ceiling = rows
        .first()
        .ok_or_else(|| error("delivery ceiling query returned no row"))?
        .get_named::<i64>("ceiling")
        .map_err(error)?;
    record_issued_cursor(storage_root, ceiling)?;
    // An earlier process may have synced the witness and exited before its
    // asynchronous Git publication. Requeue the existing file on startup even
    // when this database has no larger cursor to publish.
    let directory = JournalDirectory::open(storage_root, CURSOR_DIRECTORY, true).map_err(error)?;
    if read_ceiling(&directory)?.is_some() {
        crate::recovery_state::note_pending_archive_paths(
            storage_root,
            [format!("{CURSOR_DIRECTORY}/{CURSOR_FILE}")],
        );
    }
    Ok(())
}

/// Rebase reconstructed events above every issued cursor. The reset floor is
/// deliberately above the old ceiling, including when no events survived, so
/// a client at the old tail receives `CURSOR_EXPIRED` instead of an empty page or
/// `CURSOR_AHEAD`. Returns the new reset floor when an issuance witness exists.
fn reset_archive_cursors(conn: &CanonicalDbConn, storage_root: &Path) -> DbResult<Option<i64>> {
    let directory = match JournalDirectory::open(storage_root, CURSOR_DIRECTORY, false) {
        Ok(directory) => directory,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(error(cause)),
    };
    let Some(ceiling) = read_ceiling(&directory)? else {
        return Ok(None);
    };
    let rows = conn
        .query_sync(
            "SELECT COALESCE(MAX(seq), 0) AS seq FROM inbox_delivery_events",
            &[],
        )
        .map_err(error)?;
    let replay_tail: i64 = rows[0].get_named("seq").map_err(error)?;
    let floor = ceiling
        .max(replay_tail)
        .checked_add(1)
        .ok_or_else(|| error("delivery cursor space exhausted; cannot reset safely"))?;
    let new_tail = floor
        .checked_add(replay_tail)
        .ok_or_else(|| error("delivery cursor space exhausted; cannot rebase safely"))?;
    if new_tail == i64::MAX {
        return Err(error(
            "delivery cursor space exhausted; no safe resume position remains",
        ));
    }

    // The offset exceeds every existing seq, so no row can collide with a row
    // that has not yet moved. Updating the primary key avoids fabricating new
    // deliveries through the insertion/deletion triggers.
    conn.execute_sync(
        "UPDATE inbox_delivery_events SET seq = seq + ?",
        &[Value::BigInt(floor)],
    )
    .map_err(error)?;
    conn.execute_raw("DELETE FROM inbox_delivery_cursor_state")
        .map_err(error)?;
    conn.execute_sync(
        "INSERT INTO inbox_delivery_cursor_state(project_id, scope, scope_id, tail_seq, pruned_through_seq) \
         SELECT project_id, CASE WHEN agent_id IS NULL THEN 'project' ELSE 'agent' END, \
                COALESCE(agent_id, project_id), MAX(seq), ? \
         FROM inbox_delivery_events GROUP BY project_id, agent_id",
        &[Value::BigInt(floor)],
    )
    .map_err(error)?;
    // An empty inbox still needs a reset witness. Otherwise a watcher whose
    // entire history disappeared would see a fresh tail of zero.
    conn.execute_sync(
        "INSERT OR IGNORE INTO inbox_delivery_cursor_state(project_id, scope, scope_id, tail_seq, pruned_through_seq) \
         SELECT project_id, 'agent', id, ?, ? FROM agents",
        &[Value::BigInt(floor), Value::BigInt(floor)],
    )
    .map_err(error)?;
    conn.execute_sync(
        "INSERT OR IGNORE INTO inbox_delivery_cursor_state(project_id, scope, scope_id, tail_seq, pruned_through_seq) \
         SELECT id, 'project', id, ?, ? FROM projects",
        &[Value::BigInt(floor), Value::BigInt(floor)],
    )
    .map_err(error)?;
    conn.execute_sync(
        "UPDATE sqlite_sequence SET seq = ? WHERE name = 'inbox_delivery_events'",
        &[Value::BigInt(new_tail)],
    )
    .map_err(error)?;
    conn.execute_sync(
        "INSERT INTO sqlite_sequence(name, seq) SELECT 'inbox_delivery_events', ? \
         WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'inbox_delivery_events')",
        &[Value::BigInt(new_tail)],
    )
    .map_err(error)?;
    Ok(Some(floor))
}

#[derive(Debug, Default)]
pub(crate) struct RestoredState {
    pub receipts: usize,
    pub contacts: usize,
    pub checkpoint_present: bool,
    pub cursor_reset_floor: Option<i64>,
}

fn resolve_project(conn: &CanonicalDbConn, project: &ProjectIdentity) -> DbResult<i64> {
    let rows = conn
        .query_sync(
            "SELECT id, human_key FROM projects WHERE slug = ?",
            &[Value::Text(project.slug.clone())],
        )
        .map_err(error)?;
    let row = rows.first().ok_or_else(|| {
        error(format!(
            "record references missing project {}",
            project.slug
        ))
    })?;
    let key: String = row.get_named("human_key").map_err(error)?;
    if rows.len() != 1 || key != project.human_key {
        return Err(error(format!(
            "project identity disagrees for {}",
            project.slug
        )));
    }
    row.get_named("id").map_err(error)
}

fn resolve_agent(conn: &CanonicalDbConn, agent: &AgentIdentity) -> DbResult<i64> {
    let project = resolve_project(conn, &agent.project)?;
    let rows = conn
        .query_sync(
            "SELECT id FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE",
            &[Value::BigInt(project), Value::Text(agent.name.clone())],
        )
        .map_err(error)?;
    if rows.len() != 1 {
        return Err(error(format!(
            "record references missing or ambiguous agent {}/{}",
            agent.project.slug, agent.name
        )));
    }
    rows[0].get_named("id").map_err(error)
}

fn resolve_receipt_message(
    conn: &CanonicalDbConn,
    project: i64,
    receipt: &ReceiptRecord,
) -> DbResult<i64> {
    let mut messages = conn
        .query_sync(
            "SELECT id FROM messages WHERE id = ? AND project_id = ? AND created_ts = ? AND subject = ?",
            &[
                Value::BigInt(receipt.message_id),
                Value::BigInt(project),
                Value::BigInt(receipt.message_created_ts),
                Value::Text(receipt.message_subject.clone()),
            ],
        )
        .map_err(error)?;
    if messages.is_empty() {
        // Canonical message IDs can collide across generations. Reconstruction
        // retains such messages under a fresh ID; accept exactly one matching
        // stable identity.
        messages = conn
            .query_sync(
                "SELECT id FROM messages WHERE project_id = ? AND created_ts = ? AND subject = ?",
                &[
                    Value::BigInt(project),
                    Value::BigInt(receipt.message_created_ts),
                    Value::Text(receipt.message_subject.clone()),
                ],
            )
            .map_err(error)?;
    }
    if messages.len() != 1 {
        return Err(error(format!(
            "receipt message {} is missing or its archived identity changed",
            receipt.message_id
        )));
    }
    messages[0].get_named("id").map_err(error)
}

fn restore_receipt(conn: &CanonicalDbConn, receipt: &ReceiptRecord) -> DbResult<()> {
    let project = resolve_project(conn, &receipt.message_project)?;
    let agent = resolve_agent(conn, &receipt.agent)?;
    let message_id = resolve_receipt_message(conn, project, receipt)?;
    let read = receipt.read_ts.map_or(Value::Null, Value::BigInt);
    let ack = receipt.ack_ts.map_or(Value::Null, Value::BigInt);
    if receipt.shared {
        let deliveries = conn
            .query_sync(
                "SELECT 1 FROM project_mailbox_deliveries WHERE message_id = ? AND project_id = ?",
                &[Value::BigInt(message_id), Value::BigInt(project)],
            )
            .map_err(error)?;
        if deliveries.len() != 1 || resolve_project(conn, &receipt.agent.project)? != project {
            return Err(error("shared receipt has no matching project delivery"));
        }
        conn.execute_sync(
            "INSERT INTO project_mailbox_receipts(message_id, agent_id, read_ts, ack_ts) VALUES (?, ?, ?, ?) \
             ON CONFLICT(message_id, agent_id) DO UPDATE SET \
             read_ts = COALESCE(project_mailbox_receipts.read_ts, excluded.read_ts), \
             ack_ts = COALESCE(project_mailbox_receipts.ack_ts, excluded.ack_ts)",
            &[Value::BigInt(message_id), Value::BigInt(agent), read, ack],
        )
        .map_err(error)?;
    } else {
        let recipients = conn
            .query_sync(
                "SELECT 1 FROM message_recipients WHERE message_id = ? AND agent_id = ?",
                &[Value::BigInt(message_id), Value::BigInt(agent)],
            )
            .map_err(error)?;
        if recipients.is_empty() {
            return Err(error("receipt has no matching archived recipient"));
        }
        conn.execute_sync(
            "UPDATE message_recipients SET read_ts = COALESCE(read_ts, ?), ack_ts = COALESCE(ack_ts, ?) \
             WHERE message_id = ? AND agent_id = ?",
            &[read, ack, Value::BigInt(message_id), Value::BigInt(agent)],
        )
        .map_err(error)?;
    }
    Ok(())
}

fn restore_contact(
    conn: &CanonicalDbConn,
    contact: ContactRecord,
    preserve_salvage: bool,
) -> DbResult<()> {
    let status = contact.status.trim().to_ascii_lowercase();
    if !matches!(status.as_str(), "pending" | "approved" | "blocked") {
        return Err(error("contact record has an invalid status"));
    }
    let from_project = resolve_project(conn, &contact.from.project)?;
    let to_project = resolve_project(conn, &contact.to.project)?;
    let from = resolve_agent(conn, &contact.from)?;
    let to = resolve_agent(conn, &contact.to)?;
    let preserve_salvage = if preserve_salvage {
        " WHERE NOT EXISTS (SELECT 1 FROM temp.am_recovery_salvaged_contacts s \
         WHERE s.a_project_id = excluded.a_project_id AND s.a_agent_id = excluded.a_agent_id \
         AND s.b_project_id = excluded.b_project_id AND s.b_agent_id = excluded.b_agent_id)"
    } else {
        ""
    };
    conn.execute_sync(
        &format!("INSERT INTO agent_links(a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(a_project_id, a_agent_id, b_project_id, b_agent_id) DO UPDATE SET \
         status = excluded.status, reason = excluded.reason, created_ts = excluded.created_ts, \
         updated_ts = excluded.updated_ts, expires_ts = excluded.expires_ts{preserve_salvage}"),
        &[
            Value::BigInt(from_project),
            Value::BigInt(from),
            Value::BigInt(to_project),
            Value::BigInt(to),
            Value::Text(status),
            Value::Text(contact.reason),
            Value::BigInt(contact.created_ts),
            Value::BigInt(contact.updated_ts),
            contact.expires_ts.map_or(Value::Null, Value::BigInt),
        ],
    )
    .map_err(error)?;
    Ok(())
}

/// Apply a validated complete checkpoint followed by its latest per-key
/// records. All writes belong to the candidate's existing transaction: a bad
/// record makes the whole candidate ineligible for promotion.
pub(crate) fn restore(
    conn: &CanonicalDbConn,
    storage_root: &Path,
    reset_cursors: bool,
) -> DbResult<RestoredState> {
    let mut restored = RestoredState::default();
    if !reset_cursors {
        // A readable salvage source is authoritative for its existing contact
        // decisions. Remember those exact keys before replay: archive-only
        // keys must still accept their later incremental records even when a
        // clock moved backwards between approval and block.
        conn.execute_raw(
            "CREATE TEMP TABLE am_recovery_salvaged_contacts AS \
             SELECT a_project_id,a_agent_id,b_project_id,b_agent_id FROM agent_links",
        )
        .map_err(error)?;
    }
    let manifest = crate::recovery_state::visit_records(storage_root, |record| {
        match record {
            RecoveryRecord::Project(_) | RecoveryRecord::Agent(_) => {
                // The reconstruction identity pass consumes these before
                // messages and their dependent coordination rows are replayed.
            }
            RecoveryRecord::Receipt(receipt) => {
                restore_receipt(conn, &receipt)?;
                restored.receipts += 1;
            }
            RecoveryRecord::Contact(contact) => {
                restore_contact(conn, contact, !reset_cursors)?;
                restored.contacts += 1;
            }
        }
        Ok(())
    })?;
    restored.checkpoint_present = manifest.is_some();
    if reset_cursors {
        restored.cursor_reset_floor = reset_archive_cursors(conn, storage_root)?;
    } else {
        conn.execute_raw("DROP TABLE temp.am_recovery_salvaged_contacts")
            .map_err(error)?;
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{InboxDeliveryEventError, inbox_delivery_events_from_conn};

    fn mailbox() -> CanonicalDbConn {
        let conn = CanonicalDbConn::open_memory().unwrap();
        conn.execute_raw(crate::schema::PRAGMA_DB_INIT_BASE_SQL)
            .unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        for migration in crate::schema::schema_migrations_base() {
            if migration.id.starts_with("v25_")
                || migration.id == "v33_project_scoped_inbox_delivery_events"
                || migration.id.starts_with("v34_")
            {
                conn.execute_raw(&migration.up).unwrap();
            }
        }
        conn.execute_raw(
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES (41, 'project', '/project', 1); \
             INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES \
             (120, 41, 'Alice', 'test', 'test', 1, 1), \
             (121, 41, 'Bob', 'test', 'test', 1, 1), \
             (122, 41, 'Empty', 'test', 'test', 1, 1); \
             INSERT INTO messages(id, project_id, sender_id, subject, body_md, created_ts) \
             VALUES (4000, 41, 120, 'first', 'body', 10), (4001, 41, 120, 'second', 'body', 11); \
             INSERT INTO message_recipients(message_id, agent_id, kind) VALUES (4000, 121, 'to'), (4001, 121, 'to');",
        ).unwrap();
        conn
    }

    #[test]
    fn issued_cursor_ceiling_is_durable_and_never_moves_backwards() {
        let directory = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for cursor in [100, 90_000, 400, 12_000, 1] {
                let root = directory.path();
                scope.spawn(move || record_issued_cursor(root, cursor).unwrap());
            }
        });
        let state = JournalDirectory::open(directory.path(), CURSOR_DIRECTORY, false).unwrap();
        assert_eq!(read_ceiling(&state).unwrap(), Some(90_000));
        record_issued_cursor(directory.path(), 2).unwrap();
        assert_eq!(read_ceiling(&state).unwrap(), Some(90_000));
        assert!(record_issued_cursor(directory.path(), -1).is_err());
    }

    #[test]
    fn archive_reset_expires_old_tail_and_empty_inbox_without_reusing_sequences() {
        let directory = tempfile::tempdir().unwrap();
        let conn = mailbox();
        record_issued_cursor(directory.path(), 90_000).unwrap();
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        let floor = reset_archive_cursors(&conn, directory.path())
            .unwrap()
            .unwrap();
        conn.execute_raw("COMMIT").unwrap();
        assert_eq!(floor, 90_001);

        for agent in [121, 122] {
            for old in [0, 1, 89_999, 90_000] {
                assert!(matches!(
                    inbox_delivery_events_from_conn(&conn, 41, agent, Some(old), 20),
                    Err(InboxDeliveryEventError::CursorExpired {
                        oldest_available: 90_002,
                        ..
                    })
                ));
            }
        }
        let page = inbox_delivery_events_from_conn(&conn, 41, 121, Some(floor), 20).unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| (event.seq, event.message_id))
                .collect::<Vec<_>>(),
            vec![(90_002, 4000), (90_003, 4001)]
        );
        let empty = inbox_delivery_events_from_conn(&conn, 41, 122, Some(floor), 20).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.tail_cursor, floor);
        conn.execute_raw(
            "INSERT INTO messages(id, project_id, sender_id, subject, body_md, created_ts) \
             VALUES (4002, 41, 120, 'after recovery', 'body', 12); \
             INSERT INTO message_recipients(message_id, agent_id, kind) VALUES (4002, 121, 'to');",
        )
        .unwrap();
        let next =
            inbox_delivery_events_from_conn(&conn, 41, 121, Some(page.next_cursor), 20).unwrap();
        assert_eq!(next.events.len(), 1);
        assert_eq!(
            (next.events[0].seq, next.events[0].message_id),
            (90_004, 4002)
        );

        // A second archive-only recovery must also expire cursors issued from
        // the first recovered generation.
        record_issued_cursor(directory.path(), next.tail_cursor).unwrap();
        let again = mailbox();
        let again_floor = reset_archive_cursors(&again, directory.path())
            .unwrap()
            .unwrap();
        assert!(again_floor > next.tail_cursor);
        assert!(matches!(
            inbox_delivery_events_from_conn(&again, 41, 121, Some(next.tail_cursor), 20),
            Err(InboxDeliveryEventError::CursorExpired { .. })
        ));
    }

    #[test]
    fn checkpoint_includes_allocator_holes_and_completely_pruned_history() {
        let directory = tempfile::tempdir().unwrap();
        let conn = mailbox();
        conn.execute_raw(
            "UPDATE sqlite_sequence SET seq = 70000 WHERE name = 'inbox_delivery_events'; \
             DELETE FROM inbox_delivery_events;",
        )
        .unwrap();
        checkpoint_cursor(&conn, directory.path()).unwrap();
        let state = JournalDirectory::open(directory.path(), CURSOR_DIRECTORY, false).unwrap();
        assert_eq!(read_ceiling(&state).unwrap(), Some(70_000));
    }

    #[test]
    fn corrupt_and_exhausted_cursor_metadata_refuse_reconstruction() {
        let directory = tempfile::tempdir().unwrap();
        record_issued_cursor(directory.path(), i64::MAX).unwrap();
        let conn = mailbox();
        assert!(
            reset_archive_cursors(&conn, directory.path())
                .unwrap_err()
                .to_string()
                .contains("exhausted")
        );
        let page = inbox_delivery_events_from_conn(&conn, 41, 121, None, 20).unwrap();
        assert_eq!(page.tail_cursor, 2);
        std::fs::write(
            directory.path().join(CURSOR_DIRECTORY).join(CURSOR_FILE),
            b"{partial",
        )
        .unwrap();
        assert!(record_issued_cursor(directory.path(), 3).is_err());
        assert!(reset_archive_cursors(&conn, directory.path()).is_err());
        assert_eq!(
            inbox_delivery_events_from_conn(&conn, 41, 121, None, 20)
                .unwrap()
                .tail_cursor,
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn cursor_publication_refuses_symlink_and_preserves_target() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("operator-evidence");
        std::fs::write(&outside, b"keep me").unwrap();
        let state = directory.path().join(CURSOR_DIRECTORY);
        std::fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(&outside, state.join(CURSOR_FILE)).unwrap();
        assert!(record_issued_cursor(directory.path(), 123).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
    }

    fn archive(root: &Path) {
        let project = root.join("projects/project");
        let messages = project.join("messages/2026/11");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(
            project.join("project.json"),
            r#"{"id":41,"slug":"project","human_key":"/project","created_at":1}"#,
        )
        .unwrap();
        for (id, name) in [(120, "Alice"), (121, "Bob")] {
            let agent = project.join("agents").join(name);
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(
                agent.join("profile.json"),
                serde_json::json!({
                    "id":id, "project_id":41, "name":name, "program":"test", "model":"test",
                    "inception_ts":"2026-01-01T00:00:00Z", "last_active_ts":"2026-01-01T00:00:00Z"
                })
                .to_string(),
            )
            .unwrap();
        }
        for (id, to) in [(4000, "Bob"), (4001, "project:project")] {
            let metadata = serde_json::json!({
                "id":id, "from":"Alice", "to":[to], "cc":[], "bcc":[],
                "subject":format!("message {id}"), "importance":"normal", "ack_required":true,
                "created_ts":"2026-11-20T00:00:00Z", "attachments":[]
            });
            std::fs::write(
                messages.join(format!("message-{id}.md")),
                format!("---json\n{metadata}\n---\nbody\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn archive_only_rebuild_restores_checkpoint_and_later_receipts_and_contact_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        archive(&root);
        let source = directory.path().join("original.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        conn.execute_raw(
            "UPDATE message_recipients SET read_ts=100 WHERE message_id=4000 AND agent_id=121; \
             INSERT INTO agent_links(a_project_id,a_agent_id,b_project_id,b_agent_id,status,reason,created_ts,updated_ts) \
             VALUES(41,120,41,121,'approved','initial',10,20);",
        ).unwrap();
        crate::recovery_state::checkpoint_all(&conn, &root).unwrap();
        record_issued_cursor(&root, 70000).unwrap();
        conn.execute_raw(
            "UPDATE message_recipients SET ack_ts=200 WHERE message_id=4000 AND agent_id=121; \
             INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(4001,121,300,400); \
             UPDATE agent_links SET status='blocked',reason='latest block',updated_ts=15;",
        ).unwrap();
        crate::recovery_state::persist_receipts(&conn, &root, 121, &[4000, 4001]).unwrap();
        crate::recovery_state::persist_contact(&conn, &root, 120, 121).unwrap();
        drop(conn);
        // No salvage path is supplied. Move the old file out of the way to
        // make the only recovery authority the archive and its state records.
        std::fs::rename(&source, directory.path().join("quarantined.sqlite3")).unwrap();
        let rebuilt = directory.path().join("rebuilt.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&rebuilt, &root).unwrap();
        let conn = CanonicalDbConn::open_file(rebuilt.to_str().unwrap()).unwrap();
        let direct = conn.query_sync("SELECT read_ts,ack_ts FROM message_recipients WHERE message_id=4000 AND agent_id=121", &[]).unwrap();
        assert_eq!(direct[0].get_named::<i64>("read_ts").unwrap(), 100);
        assert_eq!(direct[0].get_named::<i64>("ack_ts").unwrap(), 200);
        let shared = conn.query_sync("SELECT read_ts,ack_ts FROM project_mailbox_receipts WHERE message_id=4001 AND agent_id=121", &[]).unwrap();
        assert_eq!(shared[0].get_named::<i64>("read_ts").unwrap(), 300);
        assert_eq!(shared[0].get_named::<i64>("ack_ts").unwrap(), 400);
        let inbox = conn
            .query_sync(
                "SELECT total_count,unread_count,ack_pending_count FROM inbox_stats WHERE agent_id=121",
                &[],
            )
            .unwrap();
        assert_eq!(inbox[0].get_named::<i64>("total_count").unwrap(), 1);
        assert_eq!(inbox[0].get_named::<i64>("unread_count").unwrap(), 0);
        assert_eq!(inbox[0].get_named::<i64>("ack_pending_count").unwrap(), 0);
        let contacts = conn
            .query_sync(
                "SELECT status,reason FROM agent_links WHERE a_agent_id=120 AND b_agent_id=121",
                &[],
            )
            .unwrap();
        assert_eq!(
            contacts[0].get_named::<String>("status").unwrap(),
            "blocked"
        );
        assert_eq!(
            contacts[0].get_named::<String>("reason").unwrap(),
            "latest block"
        );
        assert!(matches!(
            inbox_delivery_events_from_conn(&conn, 41, 121, Some(70000), 10),
            Err(InboxDeliveryEventError::CursorExpired { .. })
        ));
        assert!(
            conn.query_sync("PRAGMA foreign_key_check", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn readable_salvage_supplies_missing_profiles_before_archive_receipts_are_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        archive(&root);
        let source = directory.path().join("original.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        conn.execute_raw(
            "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) \
             VALUES(190,41,'Unarchived','test','test',1,1); \
             INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) \
             VALUES(4002,41,120,'DB-only message','body',1795132800000001,1); \
             INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) \
             VALUES(4002,190,'to',111,222); \
             INSERT INTO agent_links(a_project_id,a_agent_id,b_project_id,b_agent_id,status,reason,created_ts,updated_ts) \
             VALUES(41,120,41,190,'approved','old approval',10,20);",
        )
        .unwrap();
        crate::recovery_state::checkpoint_all(&conn, &root).unwrap();
        // The live source is authoritative even if its clock moved backwards.
        conn.execute_raw(
            "UPDATE agent_links SET status='blocked',reason='source block',updated_ts=7; \
             UPDATE message_recipients SET ack_ts=333 WHERE message_id=4002;",
        )
        .unwrap();
        drop(conn);
        let rebuilt = directory.path().join("rebuilt.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive_with_private_salvage(
            &rebuilt, &root, &source,
        )
        .unwrap();
        assert!(stats.recovery_checkpoint_present);
        assert!(stats.recovery_state_from_salvage);
        assert_eq!(stats.salvaged_messages, 1);
        let conn = CanonicalDbConn::open_file(rebuilt.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT r.read_ts,r.ack_ts,l.status,l.reason FROM agents a \
             JOIN message_recipients r ON r.agent_id=a.id \
             JOIN agent_links l ON l.b_agent_id=a.id WHERE a.name='Unarchived'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 111);
        assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 333);
        assert_eq!(rows[0].get_named::<String>("status").unwrap(), "blocked");
        assert_eq!(
            rows[0].get_named::<String>("reason").unwrap(),
            "source block"
        );
        assert!(
            conn.query_sync("PRAGMA foreign_key_check", &[])
                .unwrap()
                .is_empty()
        );
    }
}
