//! Effective delivery rows for the terminal explorer's bounded candidate set.
//!
//! The terminal view is an operator view when no agent is selected. It may
//! aggregate receipt state, but it must still count only eligible shared-mail
//! viewers and let a direct receipt win for the same message/viewer pair.

use std::collections::{BTreeSet, HashMap, HashSet};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::mail_explorer::ExplorerQuery;
use mcp_agent_mail_db::sqlmodel::Value;

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

/// Inbox displays omit hidden recipients; the sender's outbox keeps them.
/// A shared address remains one address, never a generated roster of viewers.
pub(super) fn recipient_names(
    conn: &DbConn,
    message_ids: &[i64],
    include_bcc: bool,
) -> Result<HashMap<i64, String>, String> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let shared = ExplorerQuery::shared_mailbox_schema_available(conn)
        .map_err(|error| format!("Explorer recipient schema: {error}"))?;
    let mut params = Vec::new();
    let ids = bind_ids(&mut params, message_ids.iter().copied());
    let visible_kinds = if include_bcc {
        ""
    } else {
        " AND mr.kind IN ('to', 'cc')"
    };
    let sql = format!(
        "SELECT mr.message_id, mr.agent_id AS raw_agent_id, a.name AS agent_name \
         FROM message_recipients mr \
         LEFT JOIN agents a ON a.id = mr.agent_id \
         WHERE mr.message_id IN ({ids}){visible_kinds} \
         ORDER BY mr.message_id ASC, \
         CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END ASC, \
         mr.agent_id ASC"
    );
    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|error| format!("Recipient lookup query: {error}"))?;
    let mut grouped: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let message_id = row
            .get_named::<i64>("message_id")
            .map_err(|error| format!("Recipient message identity: {error}"))?;
        let name = super::read_agent_label(&row, "agent_name", "raw_agent_id");
        let names = grouped.entry(message_id).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if shared {
        let sql = format!(
            "SELECT d.message_id, d.project_id AS raw_project_id, p.slug AS project_slug \
             FROM project_mailbox_deliveries d \
             JOIN messages m ON m.id = d.message_id AND m.project_id = d.project_id \
             LEFT JOIN projects p ON p.id = d.project_id \
             WHERE d.message_id IN ({ids}) ORDER BY d.message_id"
        );
        let rows = conn
            .query_sync(&sql, &params)
            .map_err(|error| format!("Shared recipient lookup query: {error}"))?;
        for row in rows {
            let id = row
                .get_named::<i64>("message_id")
                .map_err(|error| format!("Shared recipient message identity: {error}"))?;
            let project = super::read_project_slug(&row, "project_slug", "raw_project_id");
            let address = mcp_agent_mail_db::project_mailbox::project_mailbox_address(&project);
            let names = grouped.entry(id).or_default();
            if !names.contains(&address) {
                names.push(address);
            }
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(id, names)| (id, names.join(", ")))
        .collect())
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
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102,101,200,100]);
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
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [200,100]);
            assert!(screen("BlueLake").fetch_inbound(conn, Some(&HashSet::new())).unwrap().is_empty());
            conn.execute_raw("UPDATE agents SET retired_at=10 WHERE id=11").unwrap();
            let rows = screen("BlueLake").fetch_inbound(conn, Some(&candidates())).unwrap();
            assert_eq!(rows.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102,101,200]);
        });
    }

    #[test]
    fn terminal_receipt_filters_and_status_aggregation_use_the_same_deliveries() {
        fixture(|conn| {
            for (filter, expected) in [
                (AckFilter::Unread, vec![102,101,200]),
                (AckFilter::PendingAck, vec![102,200]),
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
            let incoming = recipient_names(conn, &[100,102], false).unwrap();
            assert_eq!(incoming[&100], "project:alpha");
            assert_eq!(incoming[&102], "project:alpha");
            let outgoing = recipient_names(conn, &[100,102], true).unwrap();
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
            assert_eq!(legacy.iter().map(|row| row.message_id).collect::<Vec<_>>(), [102,101]);
        });
    }
}
