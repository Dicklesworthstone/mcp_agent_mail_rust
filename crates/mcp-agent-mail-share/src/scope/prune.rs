//! Project-scope deletion without a mailbox-sized bind list.
//!
//! The caller owns one transaction for the complete scope change. Each phase
//! visits every excluded-project chunk before deleting any parent table, so
//! references between two excluded projects can cross chunk boundaries safely.
//! These are bind/allocation bounds, not deadlines on the database's work.

use super::{Conn, build_placeholders, exec};
use crate::ShareError;
use sqlmodel_core::Value;

const PROJECTS_PER_STATEMENT: usize = 128;

// Only these static identifiers/predicates enter generated SQL. Project IDs
// remain bound values, including legacy negative and zero project identities.
// Each predicate uses the same chunk once or twice (at most 256 bindings).
const PHASES: &[(&str, &str, bool)] = &[
    (
        "agent_links",
        "a_project_id IN ($ids) OR b_project_id IN ($ids)",
        true,
    ),
    (
        "project_sibling_suggestions",
        "project_a_id IN ($ids) OR project_b_id IN ($ids)",
        true,
    ),
    (
        "project_mailbox_receipts",
        "message_id IN (SELECT message_id FROM project_mailbox_deliveries WHERE project_id IN ($ids))",
        true,
    ),
    (
        "project_mailbox_receipts",
        "message_id IN (SELECT id FROM messages WHERE project_id IN ($ids)) OR agent_id IN (SELECT id FROM agents WHERE project_id IN ($ids))",
        true,
    ),
    (
        "project_mailbox_deliveries",
        "project_id IN ($ids) OR message_id IN (SELECT id FROM messages WHERE project_id IN ($ids))",
        true,
    ),
    ("inbox_delivery_events", "project_id IN ($ids)", true),
    (
        "inbox_delivery_events",
        "message_id IN (SELECT id FROM messages WHERE project_id IN ($ids)) OR agent_id IN (SELECT id FROM agents WHERE project_id IN ($ids))",
        true,
    ),
    (
        "message_recipients",
        "message_id IN (SELECT id FROM messages WHERE project_id IN ($ids)) OR agent_id IN (SELECT id FROM agents WHERE project_id IN ($ids))",
        false,
    ),
    (
        "file_reservation_releases",
        "reservation_id IN (SELECT id FROM file_reservations WHERE project_id IN ($ids))",
        true,
    ),
    ("file_reservations", "project_id IN ($ids)", false),
    ("messages", "project_id IN ($ids)", false),
    (
        "agent_deregistrations",
        "agent_id IN (SELECT id FROM agents WHERE project_id IN ($ids))",
        true,
    ),
    (
        "inbox_stats",
        "agent_id IN (SELECT id FROM agents WHERE project_id IN ($ids))",
        true,
    ),
    ("product_project_links", "project_id IN ($ids)", true),
    ("agents", "project_id IN ($ids)", false),
    ("projects", "id IN ($ids)", false),
];

fn statement(table: &str, predicate: &str, ids: &[i64]) -> (String, Vec<Value>) {
    debug_assert!(!ids.is_empty() && ids.len() <= PROJECTS_PER_STATEMENT);
    let repetitions = predicate.matches("$ids").count();
    debug_assert!((1..=2).contains(&repetitions));
    let predicate = predicate.replace("$ids", &build_placeholders(ids.len()));
    let params = (0..repetitions)
        .flat_map(|_| ids.iter().copied().map(Value::BigInt))
        .collect();
    (format!("DELETE FROM {table} WHERE {predicate}"), params)
}

/// Schema absence is a legacy case; failed inspection or a view is not.
/// A false "absent" verdict here could leave private rows in a scoped export.
fn table_exists(conn: &Conn, table: &str) -> Result<bool, ShareError> {
    let rows = conn.query_sync(
        "SELECT type FROM sqlite_master WHERE name = ? COLLATE NOCASE AND type IN ('table', 'view')",
        &[Value::Text(table.to_string())],
    ).map_err(|error| ShareError::Sqlite {
        message: format!("inspect scope table {table}: {error}"),
    })?;
    if rows.is_empty() {
        return Ok(false);
    }
    if rows.len() != 1
        || rows[0]
            .get_named::<String>("type")
            .map_err(|error| ShareError::Sqlite {
                message: format!("decode scope table {table}: {error}"),
            })?
            != "table"
    {
        return Err(ShareError::Validation {
            message: format!("scope object {table} must be a real table"),
        });
    }
    Ok(true)
}

fn check_cleared(
    conn: &Conn,
    table: &str,
    predicate: &str,
    params: &[Value],
) -> Result<(), ShareError> {
    let rows = conn
        .query_sync(
            &format!("SELECT 1 FROM {table} WHERE {predicate} LIMIT 1"),
            params,
        )
        .map_err(|error| ShareError::Sqlite {
            message: format!("verify scoped deletion from {table}: {error}"),
        })?;
    if !rows.is_empty() {
        return Err(ShareError::Validation {
            message: format!("excluded rows remain in {table}; scope transaction refused"),
        });
    }
    Ok(())
}

fn clear_matching(conn: &Conn, table: &str, predicate: &str) -> Result<(), ShareError> {
    exec(conn, &format!("DELETE FROM {table} WHERE {predicate}"), &[])?;
    check_cleared(conn, table, predicate, &[])
}

pub(super) fn apply(conn: &Conn, excluded: &[i64]) -> Result<(), ShareError> {
    if excluded.is_empty() {
        return Ok(());
    }
    if table_exists(conn, "project_mailbox_receipts")?
        && !table_exists(conn, "project_mailbox_deliveries")?
    {
        return Err(ShareError::Validation {
            message: "project mailbox receipts lack their delivery table; scope refused"
                .to_string(),
        });
    }
    // A shared event is also tied to the delivery's target project. Remove
    // it before discarding that authority, even if imported event/message
    // project fields disagree. Legacy direct-only event tables remain valid.
    if table_exists(conn, "project_mailbox_deliveries")?
        && table_exists(conn, "inbox_delivery_events")?
    {
        let predicate = "kind = 'project' AND message_id IN (SELECT message_id FROM project_mailbox_deliveries WHERE project_id IN ($ids))";
        for ids in excluded.chunks(PROJECTS_PER_STATEMENT) {
            let (sql, params) = statement("inbox_delivery_events", predicate, ids);
            exec(conn, &sql, &params)?;
            let predicate = predicate.replace("$ids", &build_placeholders(ids.len()));
            check_cleared(conn, "inbox_delivery_events", &predicate, &params)?;
        }
    }
    for &(table, predicate, optional) in PHASES {
        if optional && !table_exists(conn, table)? {
            continue;
        }
        for ids in excluded.chunks(PROJECTS_PER_STATEMENT) {
            let (sql, params) = statement(table, predicate, ids);
            exec(conn, &sql, &params)?;
            // Do not infer success from affected-row counts. Validate while
            // the source parents still exist: checking a child after deleting
            // its parents could hide a skipped deletion behind an empty join.
            let predicate = predicate.replace("$ids", &build_placeholders(ids.len()));
            check_cleared(conn, table, &predicate, &params)?;
        }
    }

    // Preserve the previous cleanup of pre-existing dangling markers and
    // recipients, plus products with no remaining selected-project links.
    if table_exists(conn, "file_reservation_releases")? {
        clear_matching(
            conn,
            "file_reservation_releases",
            "reservation_id NOT IN (SELECT id FROM file_reservations)",
        )?;
    }
    if table_exists(conn, "product_project_links")? && table_exists(conn, "products")? {
        clear_matching(
            conn,
            "products",
            "id NOT IN (SELECT DISTINCT product_id FROM product_project_links)",
        )?;
    }
    clear_matching(
        conn,
        "message_recipients",
        "agent_id NOT IN (SELECT id FROM agents)",
    )?;

    // These are runtime authorities, not viewer content. A kept project's
    // idempotency claim can cache an entire result delivered into an excluded
    // project; filtering by claim.project_id alone would leak that result.
    // Global proof nonces cannot be attributed to a selected project either.
    // No-scope exports never enter this function and preserve both tables.
    for table in ["idempotency_keys", "proof_gate_consumed_nonces"] {
        if table_exists(conn, table)? {
            clear_matching(conn, table, "1 = 1")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::apply_project_scope;
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture(root: &Path) -> PathBuf {
        let path = root.join("scope.sqlite3");
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw("PRAGMA foreign_keys = ON").unwrap();
        for sql in [
            "CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL)",
            "CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), name TEXT NOT NULL)",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), sender_id INTEGER NOT NULL REFERENCES agents(id), subject TEXT DEFAULT '', body_md TEXT DEFAULT '', ack_required INTEGER DEFAULT 0, created_ts INTEGER DEFAULT 1, recipients_json TEXT DEFAULT '{}', attachments TEXT DEFAULT '[]')",
            "CREATE TABLE message_recipients(message_id INTEGER REFERENCES messages(id), agent_id INTEGER REFERENCES agents(id), kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE file_reservations(id INTEGER PRIMARY KEY, project_id INTEGER REFERENCES projects(id), agent_id INTEGER REFERENCES agents(id))",
            "CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id), released_ts INTEGER)",
            "CREATE TABLE agent_deregistrations(agent_id INTEGER PRIMARY KEY REFERENCES agents(id), deregistered_at INTEGER)",
            "CREATE TABLE inbox_stats(agent_id INTEGER PRIMARY KEY REFERENCES agents(id), total_count INTEGER, unread_count INTEGER, ack_pending_count INTEGER, last_message_ts INTEGER)",
            "CREATE TABLE agent_links(id INTEGER PRIMARY KEY, a_project_id INTEGER REFERENCES projects(id), a_agent_id INTEGER REFERENCES agents(id), b_project_id INTEGER REFERENCES projects(id), b_agent_id INTEGER REFERENCES agents(id))",
            "CREATE TABLE project_sibling_suggestions(id INTEGER PRIMARY KEY, project_a_id INTEGER REFERENCES projects(id), project_b_id INTEGER REFERENCES projects(id))",
            "CREATE TABLE products(id INTEGER PRIMARY KEY)",
            "CREATE TABLE product_project_links(id INTEGER PRIMARY KEY, product_id INTEGER REFERENCES products(id), project_id INTEGER REFERENCES projects(id))",
        ] {
            conn.execute_raw(sql).unwrap();
        }
        for sql in [
            "INSERT INTO projects VALUES(1, 'keep', '/keep'), (2, 'drop-a', '/drop-a'), (3, 'drop-b', '/drop-b')",
            "INSERT INTO agents VALUES(11, 1, 'BlueLake'), (22, 2, 'RedFox'), (33, 3, 'GoldLeaf')",
            "INSERT INTO messages(id, project_id, sender_id) VALUES(101, 1, 11), (202, 2, 33), (303, 3, 22)",
            "INSERT INTO message_recipients VALUES(101, 11, 'to', 17, 19), (101, 22, 'bcc', NULL, NULL), (202, 11, 'cc', NULL, NULL), (303, 22, 'to', NULL, NULL)",
            "INSERT INTO file_reservations VALUES(10, 1, 11), (20, 2, 22)",
            "INSERT INTO file_reservation_releases VALUES(10, 111), (20, 222)",
            "INSERT INTO agent_deregistrations VALUES(22, 222)",
            "INSERT INTO inbox_stats VALUES(11, 2, 1, 0, 1), (22, 2, 2, 0, 1)",
            "INSERT INTO agent_links VALUES(1, 1, 11, 2, 22), (2, 2, 22, 3, 33)",
            "INSERT INTO project_sibling_suggestions VALUES(1, 1, 2)",
            "INSERT INTO products VALUES(1), (2)",
            "INSERT INTO product_project_links VALUES(1, 1, 1), (2, 2, 2)",
        ] {
            conn.execute_raw(sql).unwrap();
        }
        path
    }

    fn scalar(conn: &Conn, sql: &str) -> i64 {
        conn.query_sync(sql, &[]).unwrap()[0]
            .get_as::<i64>(0)
            .unwrap()
    }

    fn assert_kept_scope(path: &Path) {
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM projects"), 1);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM agents"), 1);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM messages"), 1);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM message_recipients"), 1);
        assert_eq!(
            scalar(
                &conn,
                "SELECT reservation_id FROM file_reservation_releases"
            ),
            10
        );
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM agent_deregistrations"),
            0
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM agent_links"), 0);
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM project_sibling_suggestions"),
            0
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM products"), 1);
        assert_eq!(
            scalar(
                &conn,
                "SELECT total_count FROM inbox_stats WHERE agent_id = 11"
            ),
            1
        );
        assert!(
            conn.query_sync("PRAGMA foreign_key_check", &[])
                .unwrap()
                .is_empty()
        );
        let rows = conn
            .query_sync(
                "SELECT kind, read_ts, ack_ts FROM message_recipients WHERE message_id = 101",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0].get_named::<String>("kind").unwrap(), "to");
        assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 17);
        assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 19);
        let rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 101", &[])
            .unwrap();
        let routing: serde_json::Value =
            serde_json::from_str(&rows[0].get_named::<String>("recipients_json").unwrap()).unwrap();
        assert_eq!(
            routing,
            serde_json::json!({"to": ["BlueLake"], "cc": [], "bcc": []})
        );
    }

    #[test]
    fn every_phase_has_a_fixed_bind_ceiling() {
        let ids: Vec<i64> = (0..PROJECTS_PER_STATEMENT)
            .map(|n| i64::try_from(n).unwrap() - 1)
            .collect();
        for &(table, predicate, _) in PHASES {
            let (sql, params) = statement(table, predicate, &ids);
            assert!(!sql.contains("$ids"));
            assert!(params.len() <= 256);
            assert_eq!(
                sql.bytes().filter(|&byte| byte == b'?').count(),
                params.len()
            );
            for chunk in params.chunks(ids.len()) {
                assert_eq!(
                    chunk,
                    ids.iter().copied().map(Value::BigInt).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn real_foreign_keys_keep_children_before_parents_and_preserve_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let result = apply_project_scope(&path, &["keep".to_string()]).unwrap();
        assert_eq!(result.removed_count, 2);
        assert_kept_scope(&path);
    }

    #[test]
    fn large_message_inventory_never_becomes_a_bind_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        // More than 32766 excluded messages. Only fixture-generated integers
        // are rendered here; the production deletion uses bound project IDs.
        for start in (1000..34000).step_by(100) {
            let messages = (start..start + 100)
                .map(|id| format!("({id},2,22)"))
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO messages(id, project_id, sender_id) VALUES {messages}"
            ))
            .unwrap();
            let recipients = (start..start + 100)
                .map(|id| format!("({id},22,'to')"))
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES {recipients}"
            ))
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM messages"), 33003);
        drop(conn);
        apply_project_scope(&path, &["keep".to_string()]).unwrap();
        assert_kept_scope(&path);
    }

    #[test]
    fn excluded_project_chunks_do_not_delete_parents_early() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        for id in 4..400 {
            conn.execute_raw(&format!(
                "INSERT INTO projects VALUES({id},'p-{id}','/p-{id}')"
            ))
            .unwrap();
        }
        // Project 399 is in a later chunk; its message references an agent
        // from the first chunk. Chunk-major deletion would violate the FK.
        conn.execute_raw("INSERT INTO messages(id, project_id, sender_id) VALUES(999,399,22)")
            .unwrap();
        conn.execute_raw("COMMIT").unwrap();
        drop(conn);
        let result = apply_project_scope(&path, &["keep".to_string()]).unwrap();
        assert_eq!(result.removed_count, 398);
        assert_kept_scope(&path);
    }

    #[test]
    fn later_phase_error_rolls_back_earlier_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        // An existing optional table with an incompatible schema must fail;
        // it is not an absent legacy table. Earlier phases already touch rows.
        conn.execute_raw(
            "ALTER TABLE agent_deregistrations RENAME COLUMN agent_id TO broken_agent_id",
        )
        .unwrap();
        drop(conn);
        assert!(apply_project_scope(&path, &["keep".to_string()]).is_err());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM projects"), 3);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM message_recipients"), 4);
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM file_reservation_releases"),
            2
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM agent_links"), 2);
    }

    fn add_project_mailbox_state(path: &Path) {
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        for sql in [
            "CREATE TABLE project_mailbox_deliveries(message_id INTEGER PRIMARY KEY REFERENCES messages(id), project_id INTEGER NOT NULL REFERENCES projects(id), kind TEXT NOT NULL, delivered_ts INTEGER NOT NULL)",
            "CREATE TABLE project_mailbox_receipts(message_id INTEGER NOT NULL REFERENCES messages(id), agent_id INTEGER NOT NULL REFERENCES agents(id), read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id))",
            "CREATE TABLE inbox_delivery_events(seq INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), agent_id INTEGER NOT NULL, message_id INTEGER NOT NULL REFERENCES messages(id), kind TEXT NOT NULL, delivered_ts INTEGER NOT NULL)",
            "INSERT INTO project_mailbox_deliveries VALUES(101,1,'cc',100), (202,2,'to',200)",
            "INSERT INTO project_mailbox_receipts VALUES(101,11,121,131), (101,22,141,151), (202,11,161,171), (202,22,181,191)",
            "INSERT INTO inbox_delivery_events VALUES(1,1,0,101,'project',100), (2,2,0,202,'project',200), (3,1,22,101,'bcc',201), (4,2,11,101,'to',202), (5,1,11,202,'to',203)",
        ] {
            conn.execute_raw(sql).unwrap();
        }
    }

    #[test]
    fn shared_delivery_and_event_cleanup_preserves_only_selected_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        add_project_mailbox_state(&path);
        apply_project_scope(&path, &["keep".to_string()]).unwrap();
        assert_kept_scope(&path);
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM project_mailbox_deliveries"),
            1
        );
        assert_eq!(
            scalar(&conn, "SELECT message_id FROM project_mailbox_deliveries"),
            101
        );
        let rows = conn
            .query_sync(
                "SELECT agent_id, read_ts, ack_ts FROM project_mailbox_receipts",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("agent_id").unwrap(), 11);
        assert_eq!(rows[0].get_named::<i64>("read_ts").unwrap(), 121);
        assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), 131);
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM inbox_delivery_events"),
            1
        );
        assert_eq!(scalar(&conn, "SELECT seq FROM inbox_delivery_events"), 1);
        assert_eq!(
            scalar(&conn, "SELECT agent_id FROM inbox_delivery_events"),
            0
        );
    }

    #[test]
    fn excluded_project_delivery_cannot_leave_receipts_on_a_kept_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        add_project_mailbox_state(&path);
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw(
            "UPDATE project_mailbox_deliveries SET project_id = 2 WHERE message_id = 101",
        )
        .unwrap();
        drop(conn);
        apply_project_scope(&path, &["keep".to_string()]).unwrap();
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM project_mailbox_deliveries"),
            0
        );
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM project_mailbox_receipts"),
            0
        );
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM inbox_delivery_events"),
            0
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM messages"), 1);
    }

    #[test]
    fn scoped_raw_snapshots_drop_cross_project_retry_payloads_but_unscoped_keep_them() {
        for scoped in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = fixture(dir.path());
            let conn = Conn::open_file(path.display().to_string()).unwrap();
            conn.execute_raw("CREATE TABLE idempotency_keys(project_id INTEGER, result_json TEXT)")
                .unwrap();
            conn.execute_raw("INSERT INTO idempotency_keys VALUES(1, 'private result delivered into excluded project')").unwrap();
            conn.execute_raw(
                "CREATE TABLE proof_gate_consumed_nonces(issuer_key TEXT, nonce TEXT)",
            )
            .unwrap();
            conn.execute_raw(
                "INSERT INTO proof_gate_consumed_nonces VALUES('issuer', 'private nonce')",
            )
            .unwrap();
            drop(conn);
            let ids = if scoped {
                vec!["keep".to_string()]
            } else {
                Vec::new()
            };
            apply_project_scope(&path, &ids).unwrap();
            let conn = Conn::open_file(path.display().to_string()).unwrap();
            let expected = i64::from(!scoped);
            assert_eq!(
                scalar(&conn, "SELECT COUNT(*) FROM idempotency_keys"),
                expected
            );
            assert_eq!(
                scalar(&conn, "SELECT COUNT(*) FROM proof_gate_consumed_nonces"),
                expected
            );
        }
    }

    #[test]
    fn optional_view_refusal_rolls_back_scope_and_preserves_private_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw("CREATE TABLE private_payloads(secret TEXT)")
            .unwrap();
        conn.execute_raw("INSERT INTO private_payloads VALUES('private original')")
            .unwrap();
        conn.execute_raw("CREATE VIEW idempotency_keys AS SELECT secret FROM private_payloads")
            .unwrap();
        drop(conn);
        assert!(apply_project_scope(&path, &["keep".to_string()]).is_err());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM projects"), 3);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM message_recipients"), 4);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM private_payloads"), 1);
    }

    #[test]
    fn readback_rejects_a_silently_ignored_child_delete_before_parent_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        conn.execute_raw("CREATE TRIGGER ignore_excluded_recipient BEFORE DELETE ON message_recipients WHEN OLD.agent_id = 22 BEGIN SELECT RAISE(IGNORE); END").unwrap();
        drop(conn);
        let error = apply_project_scope(&path, &["keep".to_string()]).unwrap_err();
        assert!(matches!(error, ShareError::Validation { .. }));
        let conn = Conn::open_file(path.display().to_string()).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM projects"), 3);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM message_recipients"), 4);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM agent_links"), 2);
    }
}
