//! Complete, bounded message and thread context from one database read snapshot.
//!
//! These resource inputs let a remote CLI render the same conversation window,
//! acknowledgements and neighbours without opening the live mailbox itself.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row, Value};

use crate::{DbError, models::ProjectRow, pool::SyncQuery};

pub const MAX_CONTEXT_ROWS: usize = 10_000;
pub const MAX_CONTEXT_MESSAGES: usize = 1_000;
pub const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextProject {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub from: String,
    pub from_program: Option<String>,
    pub from_model: Option<String>,
    pub to: Vec<String>,
    pub subject: String,
    pub topic: Option<String>,
    pub body_md: Option<String>,
    pub attachments_json: String,
    pub thread_id: Option<String>,
    pub importance: String,
    pub ack_required: bool,
    pub recipient_count: usize,
    pub acked_count: usize,
    pub created_ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjacentMessage {
    pub id: i64,
    pub from: String,
    pub subject: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadContextSnapshot {
    pub schema_version: u32,
    pub complete: bool,
    pub project: ContextProject,
    pub captured_ts: i64,
    pub thread_id: String,
    pub subject: String,
    pub since_ts: Option<i64>,
    pub limit: usize,
    pub include_bodies: bool,
    pub message_count: usize,
    pub omitted_older: usize,
    pub participants: Vec<String>,
    pub messages: Vec<ContextMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageContextSnapshot {
    pub schema_version: u32,
    pub complete: bool,
    pub project: ContextProject,
    pub captured_ts: i64,
    pub message: ContextMessage,
    pub thread_id: String,
    pub position: usize,
    pub total_in_thread: usize,
    pub previous: Option<AdjacentMessage>,
    pub next: Option<AdjacentMessage>,
}

fn sql_error(error: sqlmodel_core::Error) -> DbError {
    DbError::Sqlite(error.to_string())
}

fn query(conn: &impl SyncQuery, sql: &str, params: &[Value]) -> Result<Vec<Row>, DbError> {
    conn.query_sync(sql, params).map_err(sql_error)
}

fn bound_error() -> DbError {
    DbError::invalid(
        "context",
        "complete conversation context exceeds its 10000-row or 4 MiB limit",
    )
}

fn preflight(
    conn: &impl SyncQuery,
    sql: &str,
    params: &[Value],
    bytes: &mut i64,
) -> Result<usize, DbError> {
    let rows = query(conn, sql, params)?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::Internal("missing context bounds".into()))?;
    let count = row.get_named::<i64>("n").map_err(sql_error)?;
    let size = row.get_named::<i64>("bytes").map_err(sql_error)?;
    *bytes = bytes.checked_add(size).ok_or_else(bound_error)?;
    if count < 0
        || count > i64::try_from(MAX_CONTEXT_ROWS).unwrap_or(i64::MAX)
        || size < 0
        || *bytes > i64::try_from(MAX_CONTEXT_BYTES).unwrap_or(i64::MAX)
    {
        return Err(bound_error());
    }
    usize::try_from(count).map_err(|_| bound_error())
}

fn project(
    conn: &impl SyncQuery,
    expected: &ProjectRow,
    bytes: &mut i64,
) -> Result<ContextProject, DbError> {
    let id = expected
        .id
        .ok_or_else(|| DbError::invalid("project", "stored project required"))?;
    preflight(
        conn,
        "SELECT COUNT(*) AS n, COALESCE(SUM(COALESCE(length(CAST(slug AS BLOB)),0) + COALESCE(length(CAST(human_key AS BLOB)),0)),0) AS bytes FROM projects WHERE id=?",
        &[Value::BigInt(id)],
        bytes,
    )?;
    let rows = query(
        conn,
        "SELECT id,slug,human_key,created_at FROM projects WHERE id=?",
        &[Value::BigInt(id)],
    )?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::not_found("Project", id.to_string()))?;
    let found = ContextProject {
        id: row.get_named("id").map_err(sql_error)?,
        slug: row.get_named("slug").map_err(sql_error)?,
        human_key: row.get_named("human_key").map_err(sql_error)?,
        created_at: row.get_named("created_at").map_err(sql_error)?,
    };
    if found.id != id
        || found.slug != expected.slug
        || found.human_key != expected.human_key
        || found.created_at != expected.created_at
    {
        return Err(DbError::invalid(
            "project",
            "project identity changed during context read",
        ));
    }
    Ok(found)
}

fn membership(project_id: i64, thread: &str) -> (String, Vec<Value>) {
    let mut params = vec![Value::BigInt(project_id)];
    let clause = if let Ok(id) = thread.parse::<i64>() {
        params.push(Value::BigInt(id));
        "m.project_id=? AND (m.id=? OR m.thread_id=?)"
    } else {
        "m.project_id=? AND m.thread_id=?"
    };
    params.push(Value::Text(thread.to_string()));
    (clause.to_string(), params)
}

struct Conversation {
    messages: Vec<ContextMessage>,
    participants: Vec<String>,
}

fn conversation(
    conn: &impl SyncQuery,
    project_id: i64,
    thread: &str,
    bytes: &mut i64,
) -> Result<Conversation, DbError> {
    let (condition, params) = membership(project_id, thread);
    let count = preflight(
        conn,
        &format!(
            "SELECT COUNT(*) AS n, COALESCE(SUM(COALESCE(length(CAST(m.subject AS BLOB)),0)+COALESCE(length(CAST(m.topic AS BLOB)),0)+COALESCE(length(CAST(m.thread_id AS BLOB)),0)+COALESCE(length(CAST(m.importance AS BLOB)),0)+COALESCE(length(CAST(a.name AS BLOB)),0)+COALESCE(length(CAST(a.program AS BLOB)),0)+COALESCE(length(CAST(a.model AS BLOB)),0)),0) AS bytes FROM messages m LEFT JOIN agents a ON a.id=m.sender_id AND a.project_id=m.project_id WHERE {condition}"
        ),
        &params,
        bytes,
    )?;
    if count == 0 {
        return Err(DbError::not_found("Thread", thread));
    }
    let rows = query(
        conn,
        &format!(
            "SELECT m.id,m.project_id,m.sender_id,m.subject,m.topic,m.thread_id,m.importance,m.ack_required,m.created_ts,COALESCE(a.name,'[unknown sender]') AS sender_name,a.program,a.model FROM messages m LEFT JOIN agents a ON a.id=m.sender_id AND a.project_id=m.project_id WHERE {condition} ORDER BY m.created_ts,m.id LIMIT 10001"
        ),
        &params,
    )?;
    let mut messages = rows
        .iter()
        .map(|row| {
            Ok(ContextMessage {
                id: row.get_named("id").map_err(sql_error)?,
                project_id: row.get_named("project_id").map_err(sql_error)?,
                sender_id: row.get_named("sender_id").map_err(sql_error)?,
                from: row.get_named("sender_name").map_err(sql_error)?,
                from_program: row.get_named("program").map_err(sql_error)?,
                from_model: row.get_named("model").map_err(sql_error)?,
                to: Vec::new(),
                subject: row.get_named("subject").map_err(sql_error)?,
                topic: row.get_named("topic").map_err(sql_error)?,
                body_md: None,
                attachments_json: "[]".into(),
                thread_id: row.get_named("thread_id").map_err(sql_error)?,
                importance: row.get_named("importance").map_err(sql_error)?,
                ack_required: row.get_named::<i64>("ack_required").map_err(sql_error)? != 0,
                recipient_count: 0,
                acked_count: 0,
                created_ts: row.get_named("created_ts").map_err(sql_error)?,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    if messages.len() != count {
        return Err(DbError::Internal("context message count changed".into()));
    }
    let recipient_count = preflight(
        conn,
        &format!(
            "SELECT COUNT(*) AS n, COALESCE(SUM(COALESCE(length(CAST(mr.kind AS BLOB)),0)+CASE WHEN mr.kind='bcc' THEN 0 ELSE COALESCE(length(CAST(a.name AS BLOB)),0) END),0) AS bytes FROM message_recipients mr JOIN messages m ON m.id=mr.message_id LEFT JOIN agents a ON a.id=mr.agent_id AND a.project_id=m.project_id WHERE {condition}"
        ),
        &params,
        bytes,
    )?;
    let recipients = query(
        conn,
        &format!(
            "SELECT mr.message_id,mr.agent_id,mr.kind,CASE WHEN mr.ack_ts IS NOT NULL THEN 1 ELSE 0 END AS acked,CASE WHEN mr.kind='bcc' THEN '[redacted]' ELSE COALESCE(NULLIF(a.name,''),'[unknown recipient]') END AS name FROM message_recipients mr JOIN messages m ON m.id=mr.message_id LEFT JOIN agents a ON a.id=mr.agent_id AND a.project_id=m.project_id WHERE {condition} ORDER BY mr.message_id,CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END,name COLLATE NOCASE,name LIMIT 10001"
        ),
        &params,
    )?;
    if recipients.len() != recipient_count {
        return Err(DbError::Internal("context recipient count changed".into()));
    }
    let indexes: HashMap<_, _> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id, i))
        .collect();
    let mut participants: HashMap<String, (i64, i64)> = HashMap::new();
    for message in &messages {
        participants
            .entry(message.from.clone())
            .and_modify(|first| {
                first.0 = first.0.min(message.created_ts);
                first.1 = first.1.min(message.id);
            })
            .or_insert((message.created_ts, message.id));
    }
    let mut primary = HashSet::new();
    for row in &recipients {
        let id = row.get_named::<i64>("message_id").map_err(sql_error)?;
        let index = indexes
            .get(&id)
            .ok_or_else(|| DbError::Internal("foreign context recipient".into()))?;
        let message = &mut messages[*index];
        let kind = row.get_named::<String>("kind").map_err(sql_error)?;
        if !matches!(kind.as_str(), "to" | "cc" | "bcc") {
            return Err(DbError::invalid(
                "recipients",
                "invalid recipient kind in conversation context",
            ));
        }
        let name = row.get_named::<String>("name").map_err(sql_error)?;
        message.recipient_count += 1;
        message.acked_count += usize::from(row.get_named::<i64>("acked").map_err(sql_error)? != 0);
        if kind != "bcc" {
            let participant = if name == "[unknown recipient]" {
                format!(
                    "[unknown-agent-{}]",
                    row.get_named::<i64>("agent_id").map_err(sql_error)?
                )
            } else {
                name.clone()
            };
            participants
                .entry(participant)
                .and_modify(|first| {
                    first.0 = first.0.min(message.created_ts);
                    first.1 = first.1.min(id);
                })
                .or_insert((message.created_ts, id));
        }
        if kind == "to" {
            primary.insert(id);
            message.to.push(name);
        } else if !primary.contains(&id) {
            message.to.push(if matches!(kind.as_str(), "cc" | "bcc") {
                format!("{kind}: {name}")
            } else {
                name
            });
        }
    }
    let mut participants: Vec<_> = participants.into_iter().collect();
    participants.sort_by(|(a, at), (b, bt)| {
        at.cmp(bt)
            .then_with(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
            .then_with(|| a.cmp(b))
    });
    Ok(Conversation {
        messages,
        participants: participants.into_iter().map(|(name, _)| name).collect(),
    })
}

fn bodies(
    conn: &impl SyncQuery,
    messages: &mut [ContextMessage],
    include_bodies: bool,
    bytes: &mut i64,
) -> Result<(), DbError> {
    for chunk in messages.chunks_mut(256) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let params: Vec<_> = chunk.iter().map(|m| Value::BigInt(m.id)).collect();
        let body = if include_bodies { "body_md" } else { "''" };
        let count = preflight(
            conn,
            &format!(
                "SELECT COUNT(*) AS n, COALESCE(SUM(COALESCE(length(CAST({body} AS BLOB)),0)+COALESCE(length(CAST(attachments AS BLOB)),0)),0) AS bytes FROM messages WHERE id IN ({placeholders})"
            ),
            &params,
            bytes,
        )?;
        if count != chunk.len() {
            return Err(DbError::Internal("context body count changed".into()));
        }
        let rows = query(
            conn,
            &format!(
                "SELECT id,{body} AS body_md,attachments FROM messages WHERE id IN ({placeholders})"
            ),
            &params,
        )?;
        for row in rows {
            let id = row.get_named::<i64>("id").map_err(sql_error)?;
            let message = chunk
                .iter_mut()
                .find(|m| m.id == id)
                .ok_or_else(|| DbError::Internal("foreign context body".into()))?;
            message.body_md = if include_bodies {
                Some(row.get_named("body_md").map_err(sql_error)?)
            } else {
                None
            };
            message.attachments_json = row.get_named("attachments").map_err(sql_error)?;
            let _: Vec<serde_json::Value> = serde_json::from_str(&message.attachments_json)
                .map_err(|_| {
                    DbError::invalid("attachments", "invalid message attachment metadata")
                })?;
        }
    }
    Ok(())
}

pub fn thread_context_from_conn(
    conn: &impl SyncQuery,
    expected: &ProjectRow,
    thread: &str,
    limit: usize,
    since: Option<i64>,
    include_bodies: bool,
    now: i64,
) -> Result<ThreadContextSnapshot, DbError> {
    if thread.trim().is_empty() || limit > MAX_CONTEXT_MESSAGES {
        return Err(DbError::invalid(
            "context",
            "thread is required and context limit must be at most 1000",
        ));
    }
    crate::sync::with_read_snapshot(conn, || {
        let mut bytes = 0;
        let project = project(conn, expected, &mut bytes)?;
        let mut all = conversation(conn, project.id, thread, &mut bytes)?;
        let subject = all.messages[0].subject.clone();
        all.messages
            .retain(|m| since.is_none_or(|cutoff| m.created_ts > cutoff));
        let message_count = all.messages.len();
        let omitted_older = message_count.saturating_sub(limit);
        let mut messages = all.messages.split_off(omitted_older);
        bodies(conn, &mut messages, include_bodies, &mut bytes)?;
        Ok(ThreadContextSnapshot {
            schema_version: 1,
            complete: true,
            project,
            captured_ts: now,
            thread_id: thread.to_string(),
            subject,
            since_ts: since,
            limit,
            include_bodies,
            message_count,
            omitted_older,
            participants: all.participants,
            messages,
        })
    })
}

pub fn message_context_from_conn(
    conn: &impl SyncQuery,
    expected: &ProjectRow,
    id: i64,
    now: i64,
) -> Result<MessageContextSnapshot, DbError> {
    crate::sync::with_read_snapshot(conn, || {
        let mut bytes = 0;
        let project = project(conn, expected, &mut bytes)?;
        preflight(
            conn,
            "SELECT COUNT(*) AS n,COALESCE(SUM(COALESCE(length(CAST(thread_id AS BLOB)),0)),0) AS bytes FROM messages WHERE id=? AND project_id=?",
            &[Value::BigInt(id), Value::BigInt(project.id)],
            &mut bytes,
        )?;
        let seed = query(
            conn,
            "SELECT thread_id FROM messages WHERE id=? AND project_id=?",
            &[Value::BigInt(id), Value::BigInt(project.id)],
        )?;
        let row = seed
            .first()
            .ok_or_else(|| DbError::not_found("Message", id.to_string()))?;
        let thread_id = row
            .get_named::<Option<String>>("thread_id")
            .map_err(sql_error)?
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.to_string());
        let mut all = conversation(conn, project.id, &thread_id, &mut bytes)?;
        let index = all
            .messages
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| DbError::Internal("message is absent from its thread context".into()))?;
        let adjacent = |m: &ContextMessage| AdjacentMessage {
            id: m.id,
            from: m.from.clone(),
            subject: m.subject.clone(),
        };
        let previous = index
            .checked_sub(1)
            .and_then(|i| all.messages.get(i))
            .map(adjacent);
        let next = all.messages.get(index + 1).map(adjacent);
        let total_in_thread = all.messages.len();
        let mut message = all.messages.swap_remove(index);
        bodies(conn, std::slice::from_mut(&mut message), true, &mut bytes)?;
        Ok(MessageContextSnapshot {
            schema_version: 1,
            complete: true,
            project,
            captured_ts: now,
            message,
            thread_id,
            position: index + 1,
            total_in_thread,
            previous,
            next,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(conn: &crate::DbConn) -> ProjectRow {
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        conn.execute_raw(
            "INSERT INTO projects(id,slug,human_key,created_at) VALUES
                (1,'conversation','/conversation',10), (2,'foreign','/foreign',20);
             INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES
                (1,1,'Amber','test','model',1,1), (2,1,'Blue','test','model',1,1),
                (3,1,'Coral','test','model',1,1), (4,1,'Hidden','test','model',1,1),
                (5,2,'ForeignSender','test','model',1,1);
             INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,thread_id,ack_required) VALUES
                (101,1,1,'Root subject','root body',100,NULL,1),
                (102,1,2,'First reply','first body',200,'101',1),
                (103,1,1,'Tied reply','tied body',200,'101',0),
                (104,1,3,'Last reply','last body',300,'101',1),
                (201,2,5,'Foreign subject','foreign body',400,'101',1),
                (301,1,1,'Standalone','standalone body',50,NULL,0);
             INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) VALUES
                (101,2,'to',111,112), (101,3,'cc',113,NULL), (101,4,'bcc',114,115),
                (102,4,'bcc',NULL,NULL), (103,3,'cc',NULL,NULL),
                (104,1,'to',NULL,NULL), (201,5,'to',NULL,NULL);",
        ).unwrap();
        ProjectRow {
            id: Some(1),
            slug: "conversation".into(),
            human_key: "/conversation".into(),
            created_at: 10,
        }
    }

    fn fixture() -> (crate::DbConnGuard, ProjectRow) {
        let conn = crate::guard_db_conn(
            crate::DbConn::open_memory().unwrap(),
            "message context fixture",
        );
        let project = seed(&conn);
        (conn, project)
    }

    #[test]
    fn numeric_thread_context_keeps_root_latest_window_and_receipts() {
        let (conn, project) = fixture();
        let full = thread_context_from_conn(&conn, &project, "101", 10, None, true, 999).unwrap();
        assert!(full.complete);
        assert_eq!(full.message_count, 4);
        assert_eq!(full.omitted_older, 0);
        assert_eq!(full.subject, "Root subject");
        assert_eq!(full.captured_ts, 999);
        assert_eq!(
            full.messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [101, 102, 103, 104]
        );
        assert_eq!(full.messages[0].recipient_count, 3);
        assert_eq!(full.messages[0].acked_count, 2);
        assert_eq!(full.messages[0].to, ["Blue"]);
        assert_eq!(full.messages[1].to, ["bcc: [redacted]"]);
        assert_eq!(full.messages[2].to, ["cc: Coral"]);
        assert_eq!(full.participants, ["Amber", "Blue", "Coral"]);
        let encoded = serde_json::to_string(&full).unwrap();
        assert!(
            !encoded.contains("Hidden"),
            "BCC names must not leave the database read"
        );
        assert!(
            !encoded.contains("Foreign"),
            "numeric thread roots must remain project scoped"
        );

        let latest = thread_context_from_conn(&conn, &project, "101", 2, None, true, 999).unwrap();
        assert_eq!(latest.message_count, 4);
        assert_eq!(latest.omitted_older, 2);
        assert_eq!(
            latest
                .messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [103, 104]
        );
        assert_eq!(latest.messages[0].body_md.as_deref(), Some("tied body"));
        assert_eq!(latest.messages[1].body_md.as_deref(), Some("last body"));
        let since =
            thread_context_from_conn(&conn, &project, "101", 10, Some(200), false, 999).unwrap();
        assert_eq!(since.message_count, 1);
        assert_eq!(since.messages[0].id, 104);
        assert!(since.messages[0].body_md.is_none());
        assert_eq!(since.subject, "Root subject");
    }

    #[test]
    fn message_context_resolves_root_positions_neighbours_and_standalone_mail() {
        let (conn, project) = fixture();
        let middle = message_context_from_conn(&conn, &project, 103, 777).unwrap();
        assert_eq!(middle.thread_id, "101");
        assert_eq!(middle.position, 3);
        assert_eq!(middle.total_in_thread, 4);
        assert_eq!(middle.previous.unwrap().id, 102);
        assert_eq!(middle.next.unwrap().id, 104);
        assert_eq!(middle.message.body_md.as_deref(), Some("tied body"));
        let root = message_context_from_conn(&conn, &project, 101, 777).unwrap();
        assert_eq!(root.position, 1);
        assert!(root.previous.is_none());
        assert_eq!(root.next.unwrap().id, 102);
        let solo = message_context_from_conn(&conn, &project, 301, 777).unwrap();
        assert_eq!(solo.thread_id, "301");
        assert_eq!(solo.position, 1);
        assert_eq!(solo.total_in_thread, 1);
        assert!(solo.previous.is_none() && solo.next.is_none());
        assert!(message_context_from_conn(&conn, &project, 201, 777).is_err());
        assert!(message_context_from_conn(&conn, &project, 999, 777).is_err());
    }

    #[test]
    fn context_refuses_identity_drift_invalid_metadata_and_oversized_bodies() {
        let (conn, mut project) = fixture();
        project.created_at += 1;
        assert!(thread_context_from_conn(&conn, &project, "101", 10, None, true, 999).is_err());
        project.created_at -= 1;
        assert!(thread_context_from_conn(&conn, &project, "", 10, None, true, 999).is_err());
        assert!(
            thread_context_from_conn(
                &conn,
                &project,
                "101",
                MAX_CONTEXT_MESSAGES + 1,
                None,
                true,
                999
            )
            .is_err()
        );
        conn.execute_sync(
            "UPDATE messages SET body_md=? WHERE id=104",
            &[Value::Text("x".repeat(MAX_CONTEXT_BYTES + 1))],
        )
        .unwrap();
        let without_body =
            thread_context_from_conn(&conn, &project, "101", 1, None, false, 999).unwrap();
        assert_eq!(without_body.messages[0].id, 104);
        assert!(thread_context_from_conn(&conn, &project, "101", 1, None, true, 999).is_err());
        assert!(message_context_from_conn(&conn, &project, 104, 999).is_err());
        conn.execute_raw("UPDATE messages SET attachments='{}' WHERE id=101")
            .unwrap();
        assert!(message_context_from_conn(&conn, &project, 101, 999).is_err());
    }

    #[test]
    fn context_reads_keep_query_only_and_outer_transaction_intact() {
        let (conn, project) = fixture();
        conn.execute_raw("BEGIN; PRAGMA query_only=ON").unwrap();
        let first = message_context_from_conn(&conn, &project, 101, 999).unwrap();
        assert_eq!(first.message.acked_count, 2);
        assert!(message_context_from_conn(&conn, &project, 201, 999).is_err());
        assert_eq!(
            conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            1
        );
        conn.execute_raw("ROLLBACK; PRAGMA query_only=OFF").unwrap();
        conn.execute_raw("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(
            message_context_from_conn(&conn, &project, 101, 999)
                .unwrap()
                .message
                .acked_count,
            2
        );
    }

    #[test]
    fn context_does_not_resolve_cross_project_identity_references() {
        let (conn, project) = fixture();
        // These IDs satisfy the global foreign keys but are not identities in
        // this project. Imported or damaged rows must not reveal their names.
        conn.execute_raw(
            "UPDATE messages SET sender_id=5 WHERE id=104;
             INSERT INTO message_recipients(message_id,agent_id,kind)
             VALUES(104,5,'cc')",
        )
        .unwrap();
        let context =
            thread_context_from_conn(&conn, &project, "101", 10, None, true, 999).unwrap();
        assert_eq!(context.messages[3].from, "[unknown sender]");
        assert!(context.messages[3].from_program.is_none());
        assert!(context.messages[3].from_model.is_none());
        assert!(
            context
                .participants
                .iter()
                .any(|name| name == "[unknown-agent-5]")
        );
        assert!(
            !serde_json::to_string(&context)
                .unwrap()
                .contains("ForeignSender")
        );
    }

    #[test]
    fn context_counts_legacy_ack_presence_and_refuses_ambiguous_recipient_kinds() {
        let (conn, project) = fixture();
        conn.execute_raw(
            "UPDATE message_recipients SET ack_ts='2026-10-27T10:00:00Z'
             WHERE message_id=101 AND agent_id=3",
        )
        .unwrap();
        let message = message_context_from_conn(&conn, &project, 101, 999).unwrap();
        assert_eq!(message.message.acked_count, 3);
        conn.execute_raw(
            "UPDATE message_recipients SET kind='BCC' WHERE message_id=102 AND agent_id=4",
        )
        .unwrap();
        let error = thread_context_from_conn(&conn, &project, "101", 10, None, true, 999)
            .expect_err("unknown recipient kinds cannot silently bypass BCC redaction");
        assert!(error.to_string().contains("invalid recipient kind"));
        assert!(!error.to_string().contains("Hidden"));
    }

    #[test]
    fn context_receipts_and_rows_share_one_snapshot_across_a_peer_commit() {
        struct PeerCommit<'a> {
            reader: &'a crate::DbConn,
            writer: &'a crate::DbConn,
            fired: std::cell::Cell<bool>,
        }
        impl SyncQuery for PeerCommit<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql.contains("AS bytes FROM messages m") && !self.fired.replace(true) {
                    self.writer.execute_raw("UPDATE message_recipients SET ack_ts=999 WHERE message_id=101 AND agent_id=3")?;
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conversation.sqlite3");
        let reader = crate::guard_db_conn(
            crate::DbConn::open_file(path.display().to_string()).unwrap(),
            "context snapshot reader",
        );
        let project = seed(&reader);
        reader
            .execute_raw("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0")
            .unwrap();
        let writer = crate::guard_db_conn(
            crate::DbConn::open_file(path.display().to_string()).unwrap(),
            "context snapshot writer",
        );
        let race = PeerCommit {
            reader: &reader,
            writer: &writer,
            fired: std::cell::Cell::new(false),
        };
        let page = thread_context_from_conn(&race, &project, "101", 10, None, true, 999).unwrap();
        assert!(race.fired.get());
        assert_eq!(
            page.messages[0].acked_count, 2,
            "receipt aggregate must use the same snapshot as the rows"
        );
        let after =
            thread_context_from_conn(&reader, &project, "101", 10, None, true, 1000).unwrap();
        assert_eq!(after.messages[0].acked_count, 3);
    }
}
