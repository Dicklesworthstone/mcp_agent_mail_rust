//! Effective delivery rows for the terminal explorer's bounded candidate set.
//!
//! The terminal view is an operator view when no agent is selected. It may
//! aggregate receipt state, but it must still count only eligible shared-mail
//! viewers and let a direct receipt win for the same message/viewer pair.

use std::collections::{BTreeSet, HashMap, HashSet};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::mail_explorer::ExplorerQuery;
use mcp_agent_mail_db::pool::SyncQuery;
use mcp_agent_mail_db::sqlmodel::{Row, Value};

/// The relation has the same columns as `message_recipients`. All dynamic IDs
/// are bound and the supplied candidate set is applied inside both UNION arms,
/// not only after expanding shared viewers. Explicit parameter indices keep
/// the caller's outer WHERE bindings valid when this FROM text precedes them.
pub(super) fn source(
    conn: &DbConn,
    candidate_ids: Option<&HashSet<i64>>,
    agent_ids: Option<&BTreeSet<i64>>,
    params: &mut Vec<Value>,
) -> Result<String, String> {
    if !ExplorerQuery::shared_mailbox_schema_available(conn)
        .map_err(|error| format!("Explorer delivery schema: {error}"))?
    {
        return Ok("message_recipients".to_string());
    }
    let messages = candidate_ids.map(|ids| {
        let mut ids: Vec<_> = ids.iter().copied().collect();
        ids.sort_unstable();
        bind_ids(params, ids)
    });
    let agents = agent_ids.map(|ids| bind_ids(params, ids.iter().copied()));
    let restrict = |column: &str, ids: &Option<String>| match ids {
        None => String::new(),
        Some(ids) if ids.is_empty() => " AND 0".to_string(),
        Some(ids) => format!(" AND {column} IN ({ids})"),
    };
    let direct_messages = restrict("direct.message_id", &messages);
    let direct_agents = restrict("direct.agent_id", &agents);
    let shared_messages = restrict("d.message_id", &messages);
    let shared_agents = restrict("viewer.id", &agents);
    let visible = ExplorerQuery::shared_delivery_visibility_sql();
    Ok(format!(
        "(SELECT direct.message_id, direct.agent_id, direct.kind, direct.read_ts, direct.ack_ts \
         FROM message_recipients direct WHERE 1=1{direct_messages}{direct_agents} \
         UNION ALL \
         SELECT d.message_id, viewer.id, 'project', pr.read_ts, pr.ack_ts \
         FROM project_mailbox_deliveries d \
         JOIN messages m ON m.id = d.message_id \
         JOIN agents viewer ON viewer.project_id = d.project_id \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE {visible}{shared_messages}{shared_agents})"
    ))
}

fn bind_ids(params: &mut Vec<Value>, ids: impl IntoIterator<Item = i64>) -> String {
    ids.into_iter()
        .map(|id| {
            params.push(Value::BigInt(id));
            format!("?{}", params.len())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

const LOOKUP_BATCH: usize = 256;
const RECIPIENT_SAVEPOINT: &str = "SAVEPOINT am_explorer_recipients";
const RELEASE_RECIPIENT_SAVEPOINT: &str = "RELEASE SAVEPOINT am_explorer_recipients";

struct RecipientSnapshot<'a, C: SyncQuery> {
    conn: &'a C,
    active: bool,
}

impl<C: SyncQuery> Drop for RecipientSnapshot<'_, C> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.conn.execute_raw(RELEASE_RECIPIENT_SAVEPOINT)
        {
            tracing::warn!(%error, "terminal explorer recipient snapshot cleanup failed");
        }
    }
}

/// Inbox displays omit hidden recipients; the sender's outbox keeps them.
/// A shared address remains one address, never a generated roster of viewers.
///
/// Read selected receipts first, then resolve only their agent/project keys.
/// No name lookup joins a message or recipient table to unrelated history, and
/// no statement binds more than 256 IDs. The original caller owns the source;
/// this nested read scope neither opens a new database nor writes a receipt.
pub(super) fn recipient_names(
    conn: &impl SyncQuery,
    message_ids: &[i64],
    include_bcc: bool,
) -> Result<HashMap<i64, String>, String> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    conn.execute_raw(RECIPIENT_SAVEPOINT)
        .map_err(|error| format!("Explorer recipient snapshot: {error}"))?;
    let mut snapshot = RecipientSnapshot { conn, active: true };
    let result = recipient_names_in_snapshot(conn, message_ids, include_bcc);
    conn.execute_raw(RELEASE_RECIPIENT_SAVEPOINT)
        .map_err(|error| format!("Explorer recipient snapshot release: {error}"))?;
    snapshot.active = false;
    result
}

fn lookup_rows(
    conn: &impl SyncQuery,
    table: &str,
    key: &str,
    columns: &str,
    ids: &[i64],
    suffix: &str,
) -> Result<Vec<Row>, String> {
    // Table, columns and suffix are module-owned constants. Caller values are
    // bound exclusively, including malformed/negative legacy message IDs.
    if ids.is_empty() || ids.len() > LOOKUP_BATCH {
        return Err("Explorer recipient key lookup exceeded its batch bound".to_string());
    }
    let mut params = Vec::with_capacity(ids.len());
    let binds = bind_ids(&mut params, ids.iter().copied());
    conn.query_sync(
        &format!("SELECT {columns} FROM {table} WHERE {key} IN ({binds}){suffix}"),
        &params,
    )
    .map_err(|error| format!("Explorer recipient key lookup: {error}"))
}

fn integer(row: &Row, column: &str) -> Result<i64, String> {
    row.get_named::<i64>(column)
        .map_err(|error| format!("Explorer recipient {column}: {error}"))
}

fn labels(
    conn: &impl SyncQuery,
    ids: &BTreeSet<i64>,
    agents: bool,
) -> Result<HashMap<i64, String>, String> {
    let ids: Vec<_> = ids.iter().copied().collect();
    let mut names = HashMap::new();
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let (table, columns, id_column) = if agents {
            ("agents", "id AS raw_agent_id, name AS agent_name", "raw_agent_id")
        } else {
            ("projects", "id AS raw_project_id, slug AS project_slug", "raw_project_id")
        };
        for row in lookup_rows(conn, table, "id", columns, chunk, "")? {
            let id = integer(&row, id_column)?;
            let label = if agents {
                super::read_agent_label(&row, "agent_name", id_column)
            } else {
                super::read_project_slug(&row, "project_slug", id_column)
            };
            if chunk.binary_search(&id).is_err() || names.insert(id, label).is_some() {
                return Err("Explorer recipient label lookup returned an unrequested or duplicate ID".to_string());
            }
        }
    }
    Ok(names)
}

fn recipient_names_in_snapshot(
    conn: &impl SyncQuery,
    message_ids: &[i64],
    include_bcc: bool,
) -> Result<HashMap<i64, String>, String> {
    let shared = ExplorerQuery::shared_mailbox_schema_available(conn)
        .map_err(|error| format!("Explorer recipient schema: {error}"))?;
    let ids: Vec<_> = message_ids.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
    let mut grouped: HashMap<i64, Vec<String>> = HashMap::new();
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let suffix = if include_bcc {
            " ORDER BY message_id ASC, CASE kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END ASC, agent_id ASC"
        } else {
            " AND kind IN ('to', 'cc') ORDER BY message_id ASC, CASE kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 ELSE 3 END ASC, agent_id ASC"
        };
        let rows = lookup_rows(conn, "message_recipients", "message_id", "message_id, agent_id", chunk, suffix)?;
        let mut deliveries = Vec::with_capacity(rows.len());
        let mut agents = BTreeSet::new();
        for row in rows {
            let message = integer(&row, "message_id")?;
            let agent = integer(&row, "agent_id")?;
            if chunk.binary_search(&message).is_err() {
                return Err("Explorer recipient lookup returned an unrequested message".to_string());
            }
            deliveries.push((message, agent));
            agents.insert(agent);
        }
        let names = labels(conn, &agents, true)?;
        for (message, agent) in deliveries {
            let name = names.get(&agent).cloned().unwrap_or_else(|| format!("[unknown-agent-{agent}]"));
            let recipients = grouped.entry(message).or_default();
            if !recipients.contains(&name) {
                recipients.push(name);
            }
        }
        if shared {
            append_shared_addresses(conn, chunk, &mut grouped)?;
        }
    }
    Ok(grouped.into_iter().map(|(id, names)| (id, names.join(", "))).collect())
}

fn append_shared_addresses(
    conn: &impl SyncQuery,
    ids: &[i64],
    grouped: &mut HashMap<i64, Vec<String>>,
) -> Result<(), String> {
    let mut deliveries = HashMap::new();
    for row in lookup_rows(conn, "project_mailbox_deliveries", "message_id", "message_id, project_id", ids, "")? {
        let id = integer(&row, "message_id")?;
        let project = integer(&row, "project_id")?;
        if ids.binary_search(&id).is_err() || deliveries.insert(id, project).is_some() {
            return Err("Explorer shared recipient lookup returned an unrequested or duplicate message".to_string());
        }
    }
    if deliveries.is_empty() {
        return Ok(());
    }
    let selected: Vec<_> = deliveries.keys().copied().collect::<BTreeSet<_>>().into_iter().collect();
    let mut matched = Vec::new();
    let mut projects = BTreeSet::new();
    let mut observed = HashSet::new();
    for row in lookup_rows(conn, "messages", "id", "id, project_id", &selected, "")? {
        let id = integer(&row, "id")?;
        let project = integer(&row, "project_id")?;
        if selected.binary_search(&id).is_err() || !observed.insert(id) {
            return Err("Explorer shared recipient message lookup returned an unrequested or duplicate ID".to_string());
        }
        // Match the original join's scope check. Orphaned or cross-project
        // deliveries cannot manufacture an address for an unrelated message.
        if deliveries.get(&id) == Some(&project) {
            matched.push((id, project));
            projects.insert(project);
        }
    }
    let names = labels(conn, &projects, false)?;
    for (id, project) in matched {
        let slug = names.get(&project).cloned().unwrap_or_else(|| format!("[unknown-project-{project}]"));
        let address = mcp_agent_mail_db::project_mailbox::project_mailbox_address(&slug);
        let recipients = grouped.entry(id).or_default();
        if !recipients.contains(&address) {
            recipients.push(address);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{AckFilter, Direction, MailExplorerScreen, compute_stats};
    use super::*;

    fn fixture(test: impl FnOnce(&DbConn)) {
        let temp = tempfile::tempdir().unwrap();
        let conn = mcp_agent_mail_db::guard_db_conn(
            DbConn::open_file(temp.path().join("explorer.sqlite3").to_str().unwrap()).unwrap(),
            "shared terminal explorer fixture",
        );
        for sql in [
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER REFERENCES projects(id), name TEXT, inception_ts INTEGER, retired_at INTEGER, contact_policy TEXT)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER REFERENCES projects(id), sender_id INTEGER REFERENCES agents(id), subject TEXT, topic TEXT, body_md TEXT, importance TEXT, ack_required INTEGER, created_ts INTEGER, thread_id TEXT)",
            "CREATE TABLE message_recipients (message_id INTEGER REFERENCES messages(id), agent_id INTEGER REFERENCES agents(id), kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE project_mailbox_deliveries (message_id INTEGER PRIMARY KEY REFERENCES messages(id), project_id INTEGER REFERENCES projects(id), kind TEXT, delivered_ts INTEGER)",
            "CREATE TABLE project_mailbox_receipts (message_id INTEGER REFERENCES messages(id), agent_id INTEGER REFERENCES agents(id), read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "INSERT INTO projects VALUES (1, 'alpha'), (2, 'beta')",
            "INSERT INTO agents VALUES (10,1,'AmberFox',1,NULL,'auto'), (11,1,'BlueLake',1,NULL,'auto'), (12,1,'HiddenFox',1,NULL,'auto'), (13,1,'LateFox',10000,NULL,'auto'), (14,1,'BlockedFox',1,NULL,'BLOCK_ALL'), (15,1,'RetiredFox',1,10,'auto'), (20,2,'ZebraFox',1,NULL,'auto'), (21,2,'BlueLake',1,NULL,'auto')",
            "INSERT INTO messages VALUES (100,1,10,'shared','topic-a','shared body','high',1,100,'thread-a'), (101,1,10,'direct',NULL,'direct body','normal',0,200,NULL), (102,1,10,'both',NULL,'both body','low',1,300,'thread-a'), (200,2,20,'beta shared',NULL,'beta body','urgent',1,150,'thread-b'), (201,2,20,'mismatch',NULL,'hidden body','urgent',1,400,NULL)",
            "INSERT INTO message_recipients VALUES (100,12,'bcc',NULL,NULL), (101,11,'to',NULL,NULL), (102,11,'bcc',NULL,NULL)",
            "INSERT INTO project_mailbox_deliveries VALUES (100,1,'to',100), (102,1,'cc',300), (200,2,'to',150), (201,1,'to',400)",
            "INSERT INTO project_mailbox_receipts VALUES (100,11,7,8), (102,11,99,99), (200,11,88,88)",
        ] {
            conn.execute_raw(sql).unwrap();
        }
        test(&conn);
    }

    fn screen(name: &str) -> MailExplorerScreen {
        let mut screen = MailExplorerScreen::new();
        screen.agent_filter = name.to_string();
        screen
    }

    fn candidates() -> HashSet<i64> {
        HashSet::from([100, 101, 102, 200, 201])
    }

    #[test]
    fn terminal_inbox_includes_shared_mail_with_direct_precedence_and_private_receipts() {
        fixture(|conn| {
            let rows = screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap();
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102, 101, 200, 100]);
            assert_eq!(rows[0].ack_ts, None, "direct receipt must win");
            assert_eq!(rows[2].read_ts, None, "foreign identity's receipt is not the viewer's");
            assert_eq!(rows[3].ack_ts, Some(8));
            assert_eq!(rows[3].topic.as_deref(), Some("topic-a"));
            assert_eq!(rows[3].body_md, "shared body");
            assert_eq!(rows[3].to_agents, "project:alpha");
            assert!(rows.iter().all(|row| row.direction == Direction::Inbound));
            let stats = compute_stats(&rows);
            assert_eq!(stats.inbound_count, 4);
            assert_eq!(stats.pending_ack_count, 2);
            assert_eq!(stats.unique_agents, 3, "project addresses are not agents");
        });
    }

    #[test]
    fn terminal_shared_mail_honors_viewer_eligibility_and_candidate_bindings() {
        fixture(|conn| {
            for name in ["AmberFox", "ZebraFox", "LateFox", "BlockedFox", "RetiredFox", "MissingFox"] {
                assert!(screen(name).fetch_inbound(conn, Some(&candidates())).unwrap().is_empty(), "{name}");
            }
            let selected = HashSet::from([100, 200]);
            let rows = screen("BlueLake").fetch_inbound(conn, Some(&selected)).unwrap();
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [200, 100]);
            assert!(screen("BlueLake").fetch_inbound(conn, Some(&HashSet::new())).unwrap().is_empty());
            conn.execute_raw("UPDATE agents SET retired_at=10 WHERE id=11").unwrap();
            let rows = screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap();
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102, 101, 200]);
        });
    }

    #[test]
    fn terminal_receipt_filters_and_status_aggregation_use_the_same_deliveries() {
        fixture(|conn| {
            for (filter, expected) in [
                (AckFilter::Unread, vec![102, 101, 200]),
                (AckFilter::PendingAck, vec![102, 200]),
                (AckFilter::Acknowledged, vec![100]),
            ] {
                let mut screen = screen("BlueLake");
                screen.ack_filter = filter;
                let rows = screen.fetch_inbound(conn, Some(&candidates())).unwrap();
                assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), expected);
            }
            let global = screen("").fetch_inbound(conn, Some(&candidates())).unwrap();
            let shared = global.iter().find(|row| row.message_id == 100).unwrap();
            assert_eq!(shared.ack_ts, None, "unsettled direct BCC recipient still owes an ack");
        });
    }

    #[test]
    fn terminal_outbox_matches_unsettled_shared_obligations_without_roster_fanout() {
        fixture(|conn| {
            let mut sender = screen("ZebraFox");
            sender.ack_filter = AckFilter::PendingAck;
            let rows = sender.fetch_outbound(conn, Some(&candidates())).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].message_id, 200);
            assert_eq!(rows[0].to_agents, "project:beta");
            assert_eq!(rows[0].direction, Direction::Outbound);
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (200,21,50,60)").unwrap();
            assert!(sender.fetch_outbound(conn, Some(&candidates())).unwrap().is_empty());
            sender.ack_filter = AckFilter::Acknowledged;
            assert_eq!(sender.fetch_outbound(conn, Some(&candidates())).unwrap()[0].message_id, 200);
        });
    }

    #[test]
    fn terminal_inbox_hides_bcc_but_sender_outbox_keeps_it_and_shared_address() {
        fixture(|conn| {
            let incoming = recipient_names(conn, &[100, 102], false).unwrap();
            assert_eq!(incoming[&100], "project:alpha");
            assert_eq!(incoming[&102], "project:alpha");
            let outgoing = recipient_names(conn, &[100, 102], true).unwrap();
            assert_eq!(outgoing[&100], "HiddenFox, project:alpha");
            assert_eq!(outgoing[&102], "BlueLake, project:alpha");
            let rows = screen("AmberFox").fetch_outbound(conn, Some(&candidates())).unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows.iter().find(|row| row.message_id == 100).unwrap().to_agents, outgoing[&100]);
        });
    }

    #[test]
    fn terminal_shared_queries_are_read_only_and_do_not_invent_empty_partial_schema() {
        fixture(|conn| {
            conn.execute_raw("PRAGMA query_only=ON").unwrap();
            assert_eq!(screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap().len(), 4);
            for table in ["message_recipients", "project_mailbox_receipts"] {
                let rows = conn.query_sync(&format!("SELECT COUNT(*) AS n FROM {table}"), &[]).unwrap();
                assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 3);
            }
            conn.execute_raw("PRAGMA query_only=OFF").unwrap();
            conn.execute_raw("ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts").unwrap();
            let error = screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap_err();
            assert!(error.contains("incomplete project mailbox schema"));
            conn.execute_raw("ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries").unwrap();
            let legacy = screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap();
            assert_eq!(legacy.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102, 101]);
        });
    }

    struct Recording<'a> {
        conn: &'a DbConn,
        queries: std::cell::RefCell<Vec<(String, usize)>>,
        fail_labels: std::cell::Cell<bool>,
    }

    impl SyncQuery for Recording<'_> {
        fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, mcp_agent_mail_db::sqlmodel_core::Error> {
            self.queries.borrow_mut().push((sql.to_string(), params.len()));
            if sql.contains("FROM agents WHERE") && self.fail_labels.replace(false) {
                return Err(mcp_agent_mail_db::sqlmodel_core::Error::Cancelled);
            }
            self.conn.query_sync(sql, params)
        }

        fn execute_raw(&self, sql: &str) -> Result<(), mcp_agent_mail_db::sqlmodel_core::Error> {
            self.conn.execute_raw(sql)
        }
    }

    #[test]
    fn selected_message_and_agent_keys_are_batched_without_history_joins() {
        fixture(|conn| {
            conn.execute_raw("BEGIN").unwrap();
            for id in 1000..1600 {
                conn.execute_raw(&format!(
                    "INSERT INTO agents VALUES({id},1,'Reader{id}',1,NULL,'auto'); \
                     INSERT INTO messages VALUES({id},1,10,'batch',NULL,'body','normal',0,{id},NULL); \
                     INSERT INTO message_recipients VALUES({id},11,'to',NULL,NULL),(100,{id},'cc',NULL,NULL)"
                )).unwrap();
            }
            conn.execute_raw("COMMIT").unwrap();
            let observed = Recording {
                conn,
                queries: std::cell::RefCell::new(Vec::new()),
                fail_labels: std::cell::Cell::new(false),
            };
            let mut ids: Vec<_> = (1000..1600).rev().collect();
            ids.extend([100, 102, 200, 201, 100, 1000]);
            for include_bcc in [false, true] {
                let result = recipient_names(&observed, &ids, include_bcc).unwrap();
                assert_eq!(result.len(), 603);
                assert!(!result.contains_key(&201), "cross-project delivery acquired an address");
                for id in 1000..1600 {
                    assert_eq!(result[&id], "BlueLake");
                }
                let mut names: Vec<_> = (1000..1600).map(|id| format!("Reader{id}")).collect();
                if include_bcc { names.push("HiddenFox".to_string()); }
                names.push("project:alpha".to_string());
                assert_eq!(result[&100], names.join(", "));
                assert_eq!(result[&200], "project:beta");
            }
            let queries = observed.queries.borrow();
            assert!(queries.iter().filter(|(sql, _)| sql.contains("FROM agents WHERE")).count() >= 6);
            assert!(queries.iter().all(|(_, count)| *count <= LOOKUP_BATCH));
            assert!(queries.iter().all(|(sql, _)| !sql.to_ascii_lowercase().contains(" join ")));
        });
    }

    #[test]
    fn missing_and_blank_labels_keep_unknown_identity_placeholders() {
        fixture(|conn| {
            conn.execute_raw("PRAGMA foreign_keys=OFF; UPDATE agents SET name='   ' WHERE id=11; \
                UPDATE projects SET slug='   ' WHERE id=1; \
                INSERT INTO message_recipients VALUES(101,99999,'cc',NULL,NULL)").unwrap();
            let result = recipient_names(conn, &[101, 102, 201], true).unwrap();
            assert_eq!(result[&101], "[unknown-agent-11], [unknown-agent-99999]");
            assert_eq!(result[&102], "[unknown-agent-11], project:[unknown-project-1]");
            assert!(!result.contains_key(&201));
        });
    }

    #[test]
    fn failed_lookup_releases_only_its_nested_scope() {
        fixture(|conn| {
            conn.execute_raw("BEGIN; UPDATE agents SET name='ChangedFox' WHERE id=11").unwrap();
            let observed = Recording {
                conn,
                queries: std::cell::RefCell::new(Vec::new()),
                fail_labels: std::cell::Cell::new(true),
            };
            assert!(recipient_names(&observed, &[101], false).is_err());
            assert_eq!(recipient_names(conn, &[101], false).unwrap()[&101], "ChangedFox");
            conn.execute_raw("ROLLBACK").expect("recipient reader must not commit the caller's transaction");
            assert_eq!(recipient_names(conn, &[101], false).unwrap()[&101], "BlueLake");
        });
    }

    #[test]
    fn recipient_names_hold_one_snapshot_across_concurrent_rename() {
        struct Rename<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            changed: std::cell::Cell<bool>,
        }
        impl SyncQuery for Rename<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, mcp_agent_mail_db::sqlmodel_core::Error> {
                if sql.contains("FROM agents WHERE") && !self.changed.replace(true) {
                    self.writer.execute_raw("UPDATE agents SET name='NewFox' WHERE id=11")?;
                }
                self.reader.query_sync(sql, params)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), mcp_agent_mail_db::sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rename.sqlite3");
        let reader = mcp_agent_mail_db::guard_db_conn(DbConn::open_file(path.to_str().unwrap()).unwrap(), "recipient reader");
        reader.execute_raw("PRAGMA journal_mode=WAL; CREATE TABLE agents(id INTEGER PRIMARY KEY,name TEXT); \
            CREATE TABLE message_recipients(message_id INTEGER,agent_id INTEGER,kind TEXT); \
            INSERT INTO agents VALUES(11,'OldFox'); INSERT INTO message_recipients VALUES(101,11,'to')").unwrap();
        let writer = mcp_agent_mail_db::guard_db_conn(DbConn::open_file(path.to_str().unwrap()).unwrap(), "recipient writer");
        let interleaved = Rename { reader: &reader, writer: &writer, changed: std::cell::Cell::new(false) };
        assert_eq!(recipient_names(&interleaved, &[101], false).unwrap()[&101], "OldFox");
        assert!(interleaved.changed.get(), "writer interleaving did not run");
        assert_eq!(recipient_names(&*reader, &[101], false).unwrap()[&101], "NewFox");
    }
}
