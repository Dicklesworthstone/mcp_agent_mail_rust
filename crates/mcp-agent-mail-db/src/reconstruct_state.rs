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
    AgentIdentity, ContactRecord, Manifest, ProjectIdentity, ReceiptRecord, RecoveryRecord,
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
///
/// The global ceiling is monotonic across database generations and concurrent
/// readers. A failed write is an error, never an unprotected successful read.
pub fn record_issued_cursor(storage_root: &Path, cursor: i64) -> DbResult<()> {
    if cursor == 0 {
        return Ok(());
    }
    record_cursor_ceiling(storage_root, cursor)
}

fn record_cursor_ceiling(storage_root: &Path, cursor: i64) -> DbResult<()> {
    if cursor < 0 {
        return Err(error("cannot archive a negative delivery cursor"));
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
/// the healthy database.
///
/// Include pruned tails and the AUTOINCREMENT allocator.
/// Persist an explicit zero when no cursor has been issued; recovery must not
/// infer that state from a missing witness.
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
    record_cursor_ceiling(storage_root, ceiling)?;
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
    pub checkpoint_complete: bool,
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

/// Apply a validated baseline followed by its latest per-key records. Receipt
/// authority additionally requires an unchanged, completed publication witness
/// around the replay. Missing or invalid witnesses still permit inspection.
/// All writes belong to the candidate's existing transaction: a bad
/// record or a different checkpoint from identity discovery makes the whole
/// candidate ineligible for promotion. Compare explicit absence too, before
/// changing delivery cursor state.
pub(crate) fn restore(
    conn: &CanonicalDbConn,
    storage_root: &Path,
    reset_cursors: bool,
    expected_manifest: Option<&Manifest>,
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
    let publication_before = crate::recovery_state::read_revision_witness(storage_root)
        .ok()
        .flatten();
    let records = crate::recovery_state::visit_records_with_inventory(storage_root, |record| {
        match record {
            RecoveryRecord::Project(project) => {
                let recovered_id = resolve_project(conn, &project)?;
                if reset_cursors && recovered_id != project.id {
                    return Err(error(format!(
                        "checkpoint project {} has recovered numeric id {recovered_id}, expected {}",
                        project.slug, project.id
                    )));
                }
            }
            RecoveryRecord::Agent(agent) => {
                // A checkpoint can name an identity with no receipt/contact
                // references. Verify it too: a missing project or profile
                // must not turn a partial archive into a complete recovery.
                let project_id = resolve_project(conn, &agent.project)?;
                let recovered_id = resolve_agent(conn, &agent)?;
                if reset_cursors && (project_id != agent.project.id || recovered_id != agent.id) {
                    return Err(error(format!(
                        "checkpoint agent {}/{} has recovered numeric identity {project_id}/{recovered_id}, expected {}/{}",
                        agent.project.slug, agent.name, agent.project.id, agent.id
                    )));
                }
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
    let (manifest, inventory) = records.unzip();
    if manifest.as_ref() != expected_manifest {
        return Err(error(
            "active checkpoint changed between identity discovery and state replay",
        ));
    }
    restored.checkpoint_present = manifest.is_some();
    let publication_after = crate::recovery_state::read_revision_witness(storage_root)
        .ok()
        .flatten();
    restored.checkpoint_complete = manifest.as_ref().is_some_and(|manifest| {
        publication_before.as_ref().is_some_and(|before| {
            before.matches_manifest(manifest) && publication_after.as_ref() == Some(before)
        })
    });
    if restored.checkpoint_complete
        && !publication_before.as_ref().is_some_and(|before| {
            inventory
                .as_ref()
                .is_some_and(|inventory| before.matches_inventory(inventory))
        })
    {
        return Err(error(
            "incremental recovery records differ from the completed publication",
        ));
    }
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

    fn empty_mailbox() -> CanonicalDbConn {
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
        conn
    }

    fn mailbox() -> CanonicalDbConn {
        let conn = empty_mailbox();
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
    fn healthy_empty_checkpoint_publishes_explicit_zero_cursor_witness() {
        let directory = tempfile::tempdir().unwrap();
        let source = empty_mailbox();
        record_issued_cursor(directory.path(), 0).unwrap();
        assert!(!directory.path().join(CURSOR_DIRECTORY).exists());

        checkpoint_cursor(&source, directory.path()).unwrap();
        let state = JournalDirectory::open(directory.path(), CURSOR_DIRECTORY, false).unwrap();
        assert_eq!(read_ceiling(&state).unwrap(), Some(0));
        let candidate = empty_mailbox();
        assert_eq!(
            reset_archive_cursors(&candidate, directory.path()).unwrap(),
            Some(1),
            "an explicit zero witness permits a safe recovery reset"
        );

        record_issued_cursor(directory.path(), 70_000).unwrap();
        checkpoint_cursor(&source, directory.path()).unwrap();
        assert_eq!(
            read_ceiling(&state).unwrap(),
            Some(70_000),
            "a healthy empty source must not lower a prior issued ceiling"
        );
    }

    #[test]
    fn complete_checkpoint_without_cursor_witness_requires_explicit_recovery_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        let project = root.join("projects/project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.json"),
            r#"{"id":41,"slug":"project","human_key":"/project","created_at":1}"#,
        )
        .unwrap();
        let source = directory.path().join("healthy.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
        let witness = root.join(CURSOR_DIRECTORY).join(CURSOR_FILE);
        assert!(!witness.exists());

        let unprotected = directory.path().join("without-witness.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&unprotected, &root).unwrap();
        assert!(stats.recovery_checkpoint_present);
        assert!(stats.has_receipt_state());
        assert_eq!(stats.delivery_cursor_reset_floor, None);
        let refusal = stats
            .require_recovery_state()
            .expect_err("a receipt checkpoint cannot establish a missing cursor ceiling");
        assert!(refusal.to_string().contains("cursor"), "{refusal}");
        assert!(
            !witness.exists(),
            "recovery must not invent a zero ceiling to authorize its own promotion"
        );

        checkpoint_cursor(&conn, &root).unwrap();
        let cursor_journal = JournalDirectory::open(&root, CURSOR_DIRECTORY, false).unwrap();
        assert_eq!(read_ceiling(&cursor_journal).unwrap(), Some(0));
        let protected = directory.path().join("with-witness.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&protected, &root).unwrap();
        assert!(stats.recovery_checkpoint_present);
        assert!(stats.has_receipt_state());
        assert_eq!(stats.delivery_cursor_reset_floor, Some(1));
        stats
            .require_recovery_state()
            .expect("healthy zero witness makes the checkpoint recovery admissible");
    }

    #[test]
    fn archive_checkpoint_requires_recorded_numeric_identities_without_salvage() {
        let directory = tempfile::tempdir().unwrap();
        let source = mailbox();
        let manifest = crate::recovery_state::ensure_checkpoint(&source, directory.path()).unwrap();

        for remap_project in [false, true] {
            let candidate = empty_mailbox();
            let project_id = if remap_project { 141 } else { 41 };
            candidate
                .execute_sync(
                    "INSERT INTO projects(id,slug,human_key,created_at) VALUES(?,'project','/project',1)",
                    &[Value::BigInt(project_id)],
                )
                .unwrap();
            for (id, name) in [(220, "Alice"), (221, "Bob"), (222, "Empty")] {
                candidate
                    .execute_sync(
                        "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) \
                         VALUES(?,?,?,'test','test',1,1)",
                        &[
                            Value::BigInt(id),
                            Value::BigInt(project_id),
                            Value::Text(name.to_string()),
                        ],
                    )
                    .unwrap();
            }
            let refusal = restore(&candidate, directory.path(), true, Some(&manifest))
                .expect_err("archive recovery cannot renumber checkpoint identities");
            let expected = if remap_project {
                "checkpoint project"
            } else {
                "checkpoint agent"
            };
            assert!(refusal.to_string().contains(expected), "{refusal}");
            assert!(refusal.to_string().contains("numeric"), "{refusal}");

            assert!(
                restore(&candidate, directory.path(), false, Some(&manifest))
                    .expect("readable salvage may retain its verified stable-identity remapping")
                    .checkpoint_present
            );
        }
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
        assert_eq!(empty.events, [] as [crate::sync::InboxDeliveryEvent; 0]);
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
    fn archive_recovery_refuses_missing_unreferenced_checkpoint_identity() {
        for missing_project in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("archive");
            archive(&root);
            let source = directory.path().join("original.sqlite3");
            crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
            let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
            if missing_project {
                conn.execute_raw(
                    "INSERT INTO projects(id,slug,human_key,created_at) \
                     VALUES(77,'checkpoint-only','/checkpoint-only',1)",
                )
                .unwrap();
            } else {
                conn.execute_raw(
                    "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) \
                     VALUES(190,41,'CheckpointOnly','test','test',1,1)",
                )
                .unwrap();
            }
            checkpoint_cursor(&conn, &root).unwrap();
            crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
            drop(conn);
            let source_before = std::fs::read(&source).unwrap();
            let rebuilt = directory.path().join("rebuilt.sqlite3");
            let refusal = crate::reconstruct::reconstruct_from_archive(&rebuilt, &root).expect_err(
                "every checkpoint identity must exist even without receipts or contacts",
            );
            let expected = if missing_project {
                "missing project checkpoint-only"
            } else {
                "missing or ambiguous agent project/CheckpointOnly"
            };
            assert!(refusal.to_string().contains(expected), "{refusal}");
            assert_eq!(std::fs::read(&source).unwrap(), source_before);
        }
    }

    #[test]
    fn archive_checkpoint_cannot_authorize_skipped_unread_message() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        archive(&root);
        let source = directory.path().join("original.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        let manifest = crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
        assert_eq!(
            manifest.receipts, 0,
            "the skipped message has no receipt reference"
        );
        record_issued_cursor(&root, 70_000).unwrap();
        drop(conn);
        let source_before = std::fs::read(&source).unwrap();
        let checkpoint_paths = [
            root.join(CURSOR_DIRECTORY).join("active.json"),
            root.join(CURSOR_DIRECTORY)
                .join(format!("g{}", manifest.generation))
                .join("baseline.jsonl"),
            root.join(CURSOR_DIRECTORY).join(CURSOR_FILE),
        ];
        let checkpoint_before = checkpoint_paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>();
        let damaged_message = root.join("projects/project/messages/2026/11/message-4000.md");
        std::fs::write(&damaged_message, b"unread message with damaged envelope").unwrap();
        let damaged_before = std::fs::read(&damaged_message).unwrap();

        let rebuilt = directory.path().join("rebuilt.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&rebuilt, &root)
            .expect("raw inspection still materializes the recoverable message");
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.parse_errors, 1);
        assert!(stats.recovery_checkpoint_present);
        assert!(stats.delivery_cursor_reset_floor.is_some());
        assert!(!stats.has_receipt_state());
        let refusal = stats
            .require_recovery_state()
            .expect_err("a checkpoint cannot excuse known archive message loss");
        assert!(refusal.to_string().contains("parse failures"), "{refusal}");
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        assert_eq!(std::fs::read(&damaged_message).unwrap(), damaged_before);
        assert_eq!(
            checkpoint_paths
                .iter()
                .map(|path| std::fs::read(path).unwrap())
                .collect::<Vec<_>>(),
            checkpoint_before,
            "inspection must not modify the recovery authority"
        );
    }

    fn reconstructed_receipt_state(path: &Path) -> (i64, Option<i64>, Option<i64>) {
        let conn = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT (SELECT COUNT(*) FROM messages) AS messages, \
                 (SELECT ack_ts FROM message_recipients WHERE message_id=4000 AND agent_id=121) AS direct_ack, \
                 (SELECT ack_ts FROM project_mailbox_receipts WHERE message_id=4001 AND agent_id=121) AS shared_ack",
                &[],
            )
            .unwrap();
        (
            rows[0].get_named("messages").unwrap(),
            rows[0].get_named("direct_ack").unwrap(),
            rows[0].get_named("shared_ack").unwrap(),
        )
    }

    #[test]
    fn interrupted_receipt_publication_keeps_history_without_granting_receipt_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        archive(&root);
        let source = directory.path().join("original.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
        record_issued_cursor(&root, 70_000).unwrap();
        conn.execute_raw(
            "UPDATE message_recipients SET read_ts=100,ack_ts=200 WHERE message_id=4000 AND agent_id=121; \
             INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(4001,121,300,400);",
        )
        .unwrap();

        // The real direct SELECT and durable write succeed first. Only the
        // following shared SELECT sees this connection-local broken view;
        // both committed receipt rows remain intact in the main database.
        conn.execute_raw(
            "CREATE TEMP VIEW project_mailbox_receipts AS SELECT * FROM gh406_unavailable_receipts",
        )
        .unwrap();
        let failure = crate::recovery_state::persist_receipts(&conn, &root, 121, &[4000, 4001])
            .expect_err("shared receipt publication must fail after the direct write");
        assert!(
            failure.to_string().contains("gh406_unavailable_receipts"),
            "{failure}"
        );
        assert_eq!(
            reconstructed_receipt_state(&source),
            (2, Some(200), Some(400))
        );
        assert!(
            crate::recovery_state::read_revision_witness(&root)
                .unwrap()
                .is_none()
        );
        let revision_path = root.join(CURSOR_DIRECTORY).join("read-revision.json");
        let pending_revision = std::fs::read(&revision_path).unwrap();

        let inspection = directory.path().join("incomplete.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&inspection, &root)
            .expect("an interrupted publication must still permit history inspection");
        assert_eq!(stats.messages, 2);
        assert!(stats.recovery_checkpoint_present);
        assert!(!stats.recovery_checkpoint_complete);
        assert!(!stats.has_receipt_state());
        assert!(stats.requires_lossy_recovery());
        assert!(stats.require_recovery_state().is_err());
        assert_eq!(
            reconstructed_receipt_state(&inspection),
            (2, Some(200), None),
            "the direct ACK was published; the committed shared ACK was not"
        );
        assert_eq!(std::fs::read(&revision_path).unwrap(), pending_revision);

        conn.execute_raw("DROP VIEW temp.project_mailbox_receipts")
            .unwrap();
        crate::recovery_state::persist_receipts(&conn, &root, 121, &[4000, 4001]).unwrap();
        let complete = directory.path().join("complete.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&complete, &root).unwrap();
        assert!(stats.recovery_checkpoint_present);
        assert!(stats.recovery_checkpoint_complete);
        assert!(stats.has_receipt_state());
        stats
            .require_recovery_state()
            .expect("successful retry restores receipt and promotion authority");
        assert_eq!(
            reconstructed_receipt_state(&complete),
            (2, Some(200), Some(400))
        );
    }

    #[test]
    fn missing_or_malformed_publication_witness_preserves_inspection_without_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        archive(&root);
        let source = directory.path().join("original.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&source, &root).unwrap();
        let conn = CanonicalDbConn::open_file(source.to_str().unwrap()).unwrap();
        crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
        record_issued_cursor(&root, 70_000).unwrap();
        let revision = root.join(CURSOR_DIRECTORY).join("read-revision.json");
        let complete_revision = std::fs::read(&revision).unwrap();

        for (label, malformed) in [("missing", false), ("malformed", true)] {
            std::fs::rename(
                &revision,
                directory.path().join(format!("{label}-revision.json")),
            )
            .unwrap();
            if malformed {
                std::fs::write(&revision, b"{partial").unwrap();
            }
            let candidate = directory.path().join(format!("{label}.sqlite3"));
            let stats = crate::reconstruct::reconstruct_from_archive(&candidate, &root)
                .expect("invalid publication evidence must not block valid historical messages");
            assert_eq!(stats.messages, 2);
            assert!(stats.recovery_checkpoint_present);
            assert!(!stats.recovery_checkpoint_complete);
            assert!(!stats.has_receipt_state());
            assert!(stats.require_recovery_state().is_err());
            assert_eq!(reconstructed_receipt_state(&candidate), (2, None, None));
            std::fs::write(&revision, &complete_revision).unwrap();
        }
    }

    #[test]
    fn sealed_archive_rejects_missing_and_stale_receipts_and_contact_blocks() {
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
        )
        .unwrap();
        let manifest = crate::recovery_state::ensure_checkpoint(&conn, &root).unwrap();
        record_issued_cursor(&root, 70_000).unwrap();
        crate::recovery_state::persist_receipts(&conn, &root, 121, &[4000]).unwrap();
        crate::recovery_state::persist_contact(&conn, &root, 120, 121).unwrap();
        let generation = root
            .join(CURSOR_DIRECTORY)
            .join(format!("g{}", manifest.generation));
        let record_path = |prefix: &str| {
            std::fs::read_dir(&generation)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(prefix)
                })
                .unwrap()
        };
        let direct_path = record_path("receipt-");
        let contact_path = record_path("contact-");
        let stale_direct = std::fs::read(&direct_path).unwrap();
        let stale_contact = std::fs::read(&contact_path).unwrap();
        conn.execute_raw(
            "UPDATE message_recipients SET ack_ts=200 WHERE message_id=4000 AND agent_id=121; \
             INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(4001,121,300,400); \
             UPDATE agent_links SET status='blocked',reason='latest block',updated_ts=15;",
        )
        .unwrap();
        crate::recovery_state::persist_receipts(&conn, &root, 121, &[4000, 4001]).unwrap();
        crate::recovery_state::persist_contact(&conn, &root, 120, 121).unwrap();
        let shared_path = std::fs::read_dir(&generation)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path != &direct_path
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("receipt-")
            })
            .unwrap();
        let revision_path = root.join(CURSOR_DIRECTORY).join("read-revision.json");
        let complete_revision = std::fs::read(&revision_path).unwrap();
        drop(conn);
        std::fs::rename(&source, directory.path().join("quarantined.sqlite3")).unwrap();

        let complete = directory.path().join("complete.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&complete, &root).unwrap();
        stats.require_recovery_state().unwrap();
        assert!(stats.has_receipt_state());
        assert_eq!(
            reconstructed_receipt_state(&complete),
            (2, Some(200), Some(400))
        );
        let probe = CanonicalDbConn::open_file(complete.to_str().unwrap()).unwrap();
        let contacts = probe
            .query_sync(
                "SELECT status FROM agent_links WHERE a_agent_id=120 AND b_agent_id=121",
                &[],
            )
            .unwrap();
        assert_eq!(
            contacts[0].get_named::<String>("status").unwrap(),
            "blocked"
        );
        drop(probe);

        for (label, path, stale) in [
            ("missing-direct", &direct_path, None),
            ("missing-shared", &shared_path, None),
            ("missing-block", &contact_path, None),
            ("stale-direct", &direct_path, Some(stale_direct.as_slice())),
            (
                "stale-approval",
                &contact_path,
                Some(stale_contact.as_slice()),
            ),
        ] {
            let current = std::fs::read(path).unwrap();
            std::fs::rename(
                path,
                directory.path().join(format!("{label}-preserved.json")),
            )
            .unwrap();
            if let Some(bytes) = stale {
                std::fs::write(path, bytes).unwrap();
            }
            let candidate = directory.path().join(format!("{label}.sqlite3"));
            let failure = crate::reconstruct::reconstruct_from_archive(&candidate, &root)
                .expect_err("a surviving completed witness cannot authorize a damaged record set");
            assert!(
                failure
                    .to_string()
                    .contains("record count or contents differ"),
                "{label}: {failure}"
            );
            assert_eq!(
                std::fs::read(&revision_path).unwrap(),
                complete_revision,
                "read-only reconstruction preserves the original completed evidence",
            );
            std::fs::write(path, current).unwrap();
        }

        // Version 1 has no inventory evidence. Its valid history is still
        // inspectable, but it cannot authorize receipt filters or promotion.
        let mut legacy: serde_json::Value = serde_json::from_slice(&complete_revision).unwrap();
        let fields = legacy.as_object_mut().unwrap();
        fields.insert("schema_version".to_string(), serde_json::json!(1));
        fields.remove("inventory");
        fields.remove("pending_record");
        std::fs::write(&revision_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let legacy_candidate = directory.path().join("legacy.sqlite3");
        let stats = crate::reconstruct::reconstruct_from_archive(&legacy_candidate, &root).unwrap();
        assert_eq!(
            reconstructed_receipt_state(&legacy_candidate),
            (2, Some(200), Some(400))
        );
        assert!(stats.recovery_checkpoint_present);
        assert!(!stats.recovery_checkpoint_complete);
        assert!(!stats.has_receipt_state());
        assert!(stats.require_recovery_state().is_err());
        std::fs::write(&revision_path, complete_revision).unwrap();
        let restored = directory.path().join("restored.sqlite3");
        crate::reconstruct::reconstruct_from_archive(&restored, &root)
            .unwrap()
            .require_recovery_state()
            .unwrap();
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
        assert_eq!(
            stats.salvage,
            crate::reconstruct::ReconstructionSalvage::Merged
        );
        assert_eq!(stats.salvaged_rows_skipped_unmapped, 0);
        stats.require_recovery_state().unwrap();
        assert_eq!(stats.salvaged_messages, 1);
        let conn = CanonicalDbConn::open_file(rebuilt.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT r.read_ts,r.ack_ts,l.status,l.reason,l.created_ts,l.updated_ts FROM agents a \
             JOIN message_recipients r ON r.agent_id=a.id \
             JOIN agent_links l ON l.b_agent_id=a.id WHERE a.name='Unarchived'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 111);
        assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 333);
        assert_eq!(rows[0].get_named::<i64>("created_ts").unwrap(), 10);
        assert_eq!(rows[0].get_named::<i64>("updated_ts").unwrap(), 7);
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
