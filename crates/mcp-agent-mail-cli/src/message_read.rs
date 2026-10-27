//! Explicit, authenticated reads of complete conversation context.
//!
//! No local mailbox is opened here. A single server read snapshot supplies
//! receipts, sender metadata and thread positions; identity is checked before
//! and after that snapshot so no fallback or guessed context can escape.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::message_context::{
    ContextMessage, ContextProject, MAX_CONTEXT_MESSAGES, MAX_CONTEXT_ROWS, MessageContextSnapshot,
    ThreadContextSnapshot,
};
use serde_json::Value;

use crate::context::ResolvedProject;
use crate::reservation_read::{
    ServerResourceSession, encode_resource_component, same_project, validate_project,
};
use crate::{CliError, CliResult};

fn invalid(detail: &str) -> CliError {
    CliError::Other(format!(
        "invalid server message context: {detail}; refusing unverified conversation data"
    ))
}

fn valid_project(actual: &ContextProject, expected: &ResolvedProject) -> CliResult<()> {
    if actual.id != expected.id
        || actual.slug != expected.slug
        || actual.human_key != expected.human_key
        || actual.created_at != expected.created_at
    {
        return Err(invalid("snapshot project identity mismatch"));
    }
    Ok(())
}

fn valid_message(
    message: &ContextMessage,
    project: &ResolvedProject,
    bodies: bool,
) -> CliResult<()> {
    if message.id <= 0 || message.project_id != project.id || message.sender_id <= 0 {
        return Err(invalid("message identity or project mismatch"));
    }
    if message.from.trim().is_empty()
        || message.acked_count > message.recipient_count
        || message.recipient_count > MAX_CONTEXT_ROWS
        || message.body_md.is_some() != bodies
    {
        return Err(invalid(
            "incomplete sender, body or acknowledgement context",
        ));
    }
    if message.to.iter().any(|name| {
        name.trim().is_empty() || (name.starts_with("bcc: ") && name != "bcc: [redacted]")
    }) {
        return Err(invalid("invalid or unredacted recipient context"));
    }
    if message.to.len() > message.recipient_count
        || message.to.is_empty() != (message.recipient_count == 0)
    {
        return Err(invalid("recipient counts disagree"));
    }
    serde_json::from_str::<Vec<Value>>(&message.attachments_json)
        .map_err(|_| invalid("invalid attachment metadata"))?;
    Ok(())
}

fn belongs_to(message: &ContextMessage, thread: &str) -> bool {
    message.thread_id.as_deref() == Some(thread) || thread.parse::<i64>().ok() == Some(message.id)
}

fn validate_thread(
    value: Value,
    project: &ResolvedProject,
    thread: &str,
    limit: usize,
    since: Option<i64>,
    bodies: bool,
) -> CliResult<ThreadContextSnapshot> {
    let snapshot: ThreadContextSnapshot = serde_json::from_value(value)
        .map_err(|_| invalid("missing or invalid complete thread context fields"))?;
    valid_project(&snapshot.project, project)?;
    if snapshot.schema_version != 1
        || !snapshot.complete
        || snapshot.thread_id != thread
        || snapshot.limit != limit
        || snapshot.since_ts != since
        || snapshot.include_bodies != bodies
    {
        return Err(invalid("thread request or completeness marker mismatch"));
    }
    if snapshot.message_count > MAX_CONTEXT_ROWS
        || snapshot.messages.len() != snapshot.message_count.min(limit)
        || snapshot.omitted_older != snapshot.message_count.saturating_sub(limit)
    {
        return Err(invalid("thread window does not match its complete count"));
    }
    let mut ids = HashSet::new();
    let mut previous = None;
    for message in &snapshot.messages {
        valid_message(message, project, bodies)?;
        let order = (message.created_ts, message.id);
        if !belongs_to(message, thread)
            || !ids.insert(message.id)
            || previous.is_some_and(|last| order <= last)
            || since.is_some_and(|cutoff| message.created_ts <= cutoff)
        {
            return Err(invalid("foreign, duplicate or unordered thread message"));
        }
        previous = Some(order);
    }
    let mut participants = HashSet::new();
    if snapshot.participants.is_empty()
        || snapshot.participants.len() > MAX_CONTEXT_ROWS.saturating_mul(2)
        || snapshot
            .participants
            .iter()
            .any(|name| name.trim().is_empty() || !participants.insert(name))
    {
        return Err(invalid("invalid thread participants"));
    }
    if snapshot
        .messages
        .iter()
        .any(|message| !participants.contains(&message.from))
    {
        return Err(invalid("thread participants omit a visible sender"));
    }
    Ok(snapshot)
}

fn validate_message(
    value: Value,
    project: &ResolvedProject,
    id: i64,
) -> CliResult<MessageContextSnapshot> {
    let snapshot: MessageContextSnapshot = serde_json::from_value(value)
        .map_err(|_| invalid("missing or invalid complete message context fields"))?;
    valid_project(&snapshot.project, project)?;
    valid_message(&snapshot.message, project, true)?;
    let thread = snapshot
        .message
        .thread_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .map_or_else(|| id.to_string(), str::to_string);
    if snapshot.schema_version != 1
        || !snapshot.complete
        || snapshot.message.id != id
        || snapshot.thread_id != thread
        || snapshot.position == 0
        || snapshot.position > snapshot.total_in_thread
        || snapshot.total_in_thread > MAX_CONTEXT_ROWS
        || snapshot.previous.is_some() != (snapshot.position > 1)
        || snapshot.next.is_some() != (snapshot.position < snapshot.total_in_thread)
    {
        return Err(invalid(
            "message identity, completeness or thread position mismatch",
        ));
    }
    let mut ids = HashSet::from([id]);
    for adjacent in [&snapshot.previous, &snapshot.next].into_iter().flatten() {
        if adjacent.id <= 0 || adjacent.from.trim().is_empty() || !ids.insert(adjacent.id) {
            return Err(invalid("invalid or duplicate adjacent message"));
        }
    }
    Ok(snapshot)
}

async fn project_before(
    session: &ServerResourceSession,
    key: &str,
) -> CliResult<(String, ResolvedProject)> {
    if key.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "server message reads require a project path or slug".into(),
        ));
    }
    let uri = format!(
        "resource://project/{}?format=json",
        encode_resource_component(key.trim())
    );
    let project = validate_project(&session.read(&uri).await?, key)?;
    Ok((uri, project))
}

async fn project_after(
    session: &ServerResourceSession,
    uri: &str,
    key: &str,
    project: &ResolvedProject,
) -> CliResult<()> {
    let after = validate_project(&session.read(uri).await?, key)?;
    if !same_project(project, &after) {
        return Err(invalid("project identity changed during message read"));
    }
    Ok(())
}

pub(crate) fn read_thread(
    config: &Config,
    project_key: &str,
    thread: &str,
    limit: usize,
    since: Option<i64>,
    include_bodies: bool,
) -> CliResult<(ResolvedProject, ThreadContextSnapshot)> {
    if thread.trim().is_empty() || limit > MAX_CONTEXT_MESSAGES {
        return Err(CliError::InvalidArgument(
            "server thread reads require a thread id and --limit from 0 through 1000".into(),
        ));
    }
    let session = ServerResourceSession::new(config)?;
    crate::context::run_async(async {
        let (project_uri, project) = project_before(&session, project_key).await?;
        let mut uri = format!(
            "resource://thread/{}?project={}&context=true&limit={limit}&include_bodies={include_bodies}&format=json",
            encode_resource_component(thread),
            encode_resource_component(&project.slug),
        );
        if let Some(cutoff) = since {
            uri.push_str("&since_ts=");
            uri.push_str(&encode_resource_component(
                &mcp_agent_mail_db::micros_to_iso(cutoff),
            ));
        }
        let snapshot = validate_thread(
            session.read(&uri).await?,
            &project,
            thread,
            limit,
            since,
            include_bodies,
        )?;
        project_after(&session, &project_uri, project_key, &project).await?;
        Ok((project, snapshot))
    })
}

pub(crate) fn read_message(
    config: &Config,
    project_key: &str,
    id: i64,
) -> CliResult<(ResolvedProject, MessageContextSnapshot)> {
    if id <= 0 {
        return Err(CliError::InvalidArgument(
            "message id must be positive".into(),
        ));
    }
    let session = ServerResourceSession::new(config)?;
    crate::context::run_async(async {
        let (project_uri, project) = project_before(&session, project_key).await?;
        let uri = format!(
            "resource://message/{id}?project={}&context=true&format=json",
            encode_resource_component(&project.slug)
        );
        let snapshot = validate_message(session.read(&uri).await?, &project, id)?;
        project_after(&session, &project_uri, project_key, &project).await?;
        Ok((project, snapshot))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> ResolvedProject {
        ResolvedProject {
            id: 7,
            slug: "demo".into(),
            human_key: "/work/demo".into(),
            created_at: 100,
        }
    }

    fn message(id: i64, body: bool) -> Value {
        json!({"id":id,"project_id":7,"sender_id":1,"from":"BlueLake",
            "from_program":"codex","from_model":"model","to":["RedRiver"],
            "subject":"reply","topic":null,"body_md":body.then_some("body"),
            "attachments_json":"[]","thread_id":"TKT-1","importance":"normal",
            "ack_required":true,"recipient_count":2,"acked_count":1,"created_ts":id*1000})
    }

    fn thread() -> Value {
        json!({"schema_version":1,"complete":true,
            "project":{"id":7,"slug":"demo","human_key":"/work/demo","created_at":100},
            "captured_ts":10000,"thread_id":"TKT-1","subject":"root",
            "since_ts":null,"limit":2,"include_bodies":true,"message_count":4,"omitted_older":2,
            "participants":["BlueLake","RedRiver"],"messages":[message(3,true),message(4,true)]})
    }

    fn single() -> Value {
        json!({"schema_version":1,"complete":true,
            "project":{"id":7,"slug":"demo","human_key":"/work/demo","created_at":100},
            "captured_ts":10000,"message":message(3,true),"thread_id":"TKT-1",
            "position":3,"total_in_thread":4,
            "previous":{"id":2,"from":"RedRiver","subject":"older"},
            "next":{"id":4,"from":"BlueLake","subject":"newer"}})
    }

    #[test]
    fn thread_accepts_complete_newest_window_and_rejects_partial_or_foreign_context() {
        let result = validate_thread(thread(), &project(), "TKT-1", 2, None, true).unwrap();
        assert_eq!(result.message_count, 4);
        assert_eq!(result.messages[0].id, 3);
        let mut variants = Vec::new();
        let mut bad = thread();
        bad["complete"] = json!(false);
        variants.push(bad);
        let mut bad = thread();
        bad["message_count"] = json!(5);
        variants.push(bad);
        let mut bad = thread();
        bad["project"]["created_at"] = json!(101);
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["project_id"] = json!(8);
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["thread_id"] = json!("other");
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["body_md"] = Value::Null;
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["acked_count"] = json!(3);
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][1] = message(3, true);
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["to"] = json!(["bcc: HiddenAgent"]);
        variants.push(bad);
        let mut bad = thread();
        bad["messages"][0]["to"] = json!([]);
        variants.push(bad);
        let mut bad = thread();
        bad["participants"] = json!(["RedRiver"]);
        variants.push(bad);
        for bad in variants {
            assert!(validate_thread(bad, &project(), "TKT-1", 2, None, true).is_err());
        }
    }

    #[test]
    fn message_requires_authoritative_neighbours_and_correct_project_epoch() {
        assert!(validate_message(single(), &project(), 3).is_ok());
        let mut variants = Vec::new();
        let mut bad = single();
        bad["previous"] = Value::Null;
        variants.push(bad);
        let mut bad = single();
        bad["next"]["id"] = json!(3);
        variants.push(bad);
        let mut bad = single();
        bad["next"]["id"] = json!(2);
        variants.push(bad);
        let mut bad = single();
        bad["position"] = json!(5);
        variants.push(bad);
        let mut bad = single();
        bad["project"]["created_at"] = json!(0);
        variants.push(bad);
        let mut bad = single();
        bad["thread_id"] = json!("another");
        variants.push(bad);
        for bad in variants {
            assert!(validate_message(bad, &project(), 3).is_err());
        }
    }
}
