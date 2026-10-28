//! Authenticated reads of complete conversation context.
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

/// Explicit selectors stay exact. Only implicit working-directory scope can
/// select a registered ancestor, after the daemon advertises that contract.
#[derive(Debug, Clone)]
pub(crate) enum ProjectSelection {
    Exact(String),
    Ancestors(String),
}

impl ProjectSelection {
    pub(crate) fn key(&self) -> &str {
        match self {
            Self::Exact(key) | Self::Ancestors(key) => key,
        }
    }

    pub(crate) const fn supported_by(&self, session: &ServerResourceSession) -> bool {
        matches!(self, Self::Exact(_)) || session.supports_ancestor_resolution()
    }

    pub(crate) fn permits_automatic_read(&self, config: &Config) -> bool {
        // Git identity modes can unite sibling worktrees/checkouts under a
        // project whose stored path is not a physical ancestor of this one.
        // Keep that existing local resolver until the daemon offers the same
        // identity contract; explicit stored slugs need no path discovery.
        !config.worktrees_enabled
            || config.project_identity_mode == mcp_agent_mail_core::ProjectIdentityMode::Dir
            || matches!(self, Self::Exact(key) if !std::path::Path::new(key.trim()).is_absolute())
    }
}

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

pub(crate) async fn project_before(
    session: &ServerResourceSession,
    selection: &ProjectSelection,
) -> CliResult<(String, ResolvedProject)> {
    let uri = project_resource_uri(selection)?;
    let (project, _) = read_project_descriptor(session, &uri, selection).await?;
    Ok((uri, project))
}

pub(crate) fn project_resource_uri(selection: &ProjectSelection) -> CliResult<String> {
    let key = selection.key();
    if key.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "server reads require a project path or slug".into(),
        ));
    }
    let request_key = project_request_key(key);
    let query = match selection {
        ProjectSelection::Exact(_) => "format=json",
        ProjectSelection::Ancestors(_) => {
            if !std::path::Path::new(&request_key).is_absolute() {
                return Err(CliError::InvalidArgument(
                    "ancestor project selection requires an absolute path".into(),
                ));
            }
            "resolve=ancestors&format=json"
        }
    };
    Ok(format!(
        "resource://project/{}?{query}",
        encode_resource_component(&request_key)
    ))
}

/// Retain the verified descriptor for readers that also validate its agent
/// roster. One RPC supplies both project and roster authority.
pub(crate) async fn read_project_descriptor(
    session: &ServerResourceSession,
    uri: &str,
    selection: &ProjectSelection,
) -> CliResult<(ResolvedProject, Value)> {
    let payload = session.read(uri).await?;
    let project = validate_project_selection(&payload, selection)?;
    Ok((project, payload))
}

fn project_request_key(key: &str) -> String {
    let key = key.trim();
    if std::path::Path::new(key).is_absolute() {
        // Match the local resolver's existing-directory aliases without Git
        // identity discovery, which can write worktree metadata on a read.
        mcp_agent_mail_core::resolve_project_path(key)
            .to_string_lossy()
            .into_owned()
    } else {
        key.to_string()
    }
}

pub(crate) async fn project_after(
    session: &ServerResourceSession,
    uri: &str,
    selection: &ProjectSelection,
    project: &ResolvedProject,
) -> CliResult<()> {
    let (after, _) = read_project_descriptor(session, uri, selection).await?;
    if !same_project(project, &after) {
        return Err(invalid("project identity changed during message read"));
    }
    Ok(())
}

fn validate_project_selection(
    value: &Value,
    selection: &ProjectSelection,
) -> CliResult<ResolvedProject> {
    let ProjectSelection::Ancestors(path) = selection else {
        if value
            .get("resolution")
            .is_some_and(|resolution| !resolution.is_null())
        {
            return Err(invalid(
                "ancestor selection returned for an exact project request",
            ));
        }
        return validate_project(value, selection.key());
    };
    let requested = project_request_key(path);
    let witness = value
        .get("resolution")
        .ok_or_else(|| invalid("missing ancestor project resolution witness"))?;
    if witness.get("kind").and_then(Value::as_str) != Some("nearest_ancestor")
        || witness.get("requested_path").and_then(Value::as_str) != Some(requested.as_str())
    {
        return Err(invalid(
            "ancestor project resolution witness does not match the request",
        ));
    }
    let human_key = value
        .get("human_key")
        .and_then(Value::as_str)
        .filter(|key| std::path::Path::new(key).is_absolute())
        .ok_or_else(|| invalid("ancestor project has no absolute human key"))?;
    let project = validate_project(value, human_key)?;
    let selected_path = project_request_key(human_key);
    if !std::path::Path::new(&requested).starts_with(&selected_path) {
        return Err(invalid(
            "selected project is not an ancestor of the requested path",
        ));
    }
    Ok(project)
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
    let selection = ProjectSelection::Exact(project_key.to_string());
    crate::context::run_async(read_thread_session(
        &session,
        &selection,
        thread,
        limit,
        since,
        include_bodies,
    ))
}

pub(crate) fn try_read_thread(
    config: &Config,
    selection: &ProjectSelection,
    thread: &str,
    limit: usize,
    since: Option<i64>,
    include_bodies: bool,
) -> CliResult<Option<(ResolvedProject, ThreadContextSnapshot)>> {
    if thread.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "thread id must not be empty".into(),
        ));
    }
    // Preserve larger offline requests without weakening the server's bounded
    // conversation snapshot contract.
    if limit > MAX_CONTEXT_MESSAGES || !selection.permits_automatic_read(config) {
        return Ok(None);
    }
    crate::context::run_async(async {
        let Some(session) = ServerResourceSession::for_local_mailbox(config).await? else {
            return Ok(None);
        };
        if !selection.supported_by(&session) {
            return Ok(None);
        }
        let result =
            read_thread_session(&session, selection, thread, limit, since, include_bodies).await?;
        session.verify_local_mailbox(config).await?;
        Ok(Some(result))
    })
}

async fn read_thread_session(
    session: &ServerResourceSession,
    selection: &ProjectSelection,
    thread: &str,
    limit: usize,
    since: Option<i64>,
    include_bodies: bool,
) -> CliResult<(ResolvedProject, ThreadContextSnapshot)> {
    let (project_uri, project) = project_before(session, selection).await?;
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
    project_after(session, &project_uri, selection, &project).await?;
    Ok((project, snapshot))
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
    let selection = ProjectSelection::Exact(project_key.to_string());
    crate::context::run_async(read_message_session(&session, &selection, id))
}

pub(crate) fn try_read_message(
    config: &Config,
    selection: &ProjectSelection,
    id: i64,
) -> CliResult<Option<(ResolvedProject, MessageContextSnapshot)>> {
    if id <= 0 {
        return Err(CliError::InvalidArgument(
            "message id must be positive".into(),
        ));
    }
    if !selection.permits_automatic_read(config) {
        return Ok(None);
    }
    crate::context::run_async(async {
        let Some(session) = ServerResourceSession::for_local_mailbox(config).await? else {
            return Ok(None);
        };
        if !selection.supported_by(&session) {
            return Ok(None);
        }
        let result = read_message_session(&session, selection, id).await?;
        session.verify_local_mailbox(config).await?;
        Ok(Some(result))
    })
}

async fn read_message_session(
    session: &ServerResourceSession,
    selection: &ProjectSelection,
    id: i64,
) -> CliResult<(ResolvedProject, MessageContextSnapshot)> {
    let (project_uri, project) = project_before(session, selection).await?;
    let uri = format!(
        "resource://message/{id}?project={}&context=true&format=json",
        encode_resource_component(&project.slug)
    );
    let snapshot = validate_message(session.read(&uri).await?, &project, id)?;
    project_after(session, &project_uri, selection, &project).await?;
    Ok((project, snapshot))
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

    fn project_resource() -> Value {
        json!({"id":7,"slug":"demo","human_key":"/work/demo",
            "created_at":mcp_agent_mail_db::micros_to_iso(100),"agents":[]})
    }

    #[test]
    fn automatic_git_identity_path_reads_keep_the_local_resolver() {
        use mcp_agent_mail_core::ProjectIdentityMode;

        let path = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AGENT_MAIL_TOKEN", "invalid bearer token")],
            || {
                for mode in [
                    ProjectIdentityMode::GitRemote,
                    ProjectIdentityMode::GitCommonDir,
                    ProjectIdentityMode::GitToplevel,
                ] {
                    let mut config = Config {
                        worktrees_enabled: true,
                        project_identity_mode: mode,
                        ..Config::default()
                    };
                    for selection in [
                        ProjectSelection::Ancestors(path.clone()),
                        ProjectSelection::Exact(path.clone()),
                    ] {
                        assert!(!selection.permits_automatic_read(&config));
                        // A session would reject this token, so successful
                        // fallback proves no daemon authority was selected.
                        assert!(try_read_message(&config, &selection, 3).unwrap().is_none());
                        assert!(
                            try_read_thread(&config, &selection, "TKT-1", 20, None, false)
                                .unwrap()
                                .is_none()
                        );
                        assert!(
                            crate::reservation_read::try_read_local_mailbox(
                                &config, &selection, true
                            )
                            .unwrap()
                            .is_none()
                        );
                    }
                    assert!(
                        ProjectSelection::Exact("stored-slug".into())
                            .permits_automatic_read(&config)
                    );
                    // Explicit --server still attempts its exact server read.
                    assert!(
                        read_message(&config, &path, 3)
                            .unwrap_err()
                            .to_string()
                            .contains("bearer token")
                    );
                    config.worktrees_enabled = false;
                    assert!(
                        ProjectSelection::Ancestors(path.clone()).permits_automatic_read(&config)
                    );
                }
                let config = Config {
                    worktrees_enabled: true,
                    project_identity_mode: ProjectIdentityMode::Dir,
                    ..Config::default()
                };
                assert!(ProjectSelection::Ancestors(path.clone()).permits_automatic_read(&config));
                assert!(ProjectSelection::Exact(path).permits_automatic_read(&config));
            },
        );
    }

    #[test]
    fn absolute_project_aliases_use_read_only_canonicalization() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        let dot_alias = directory.path().join(".");
        assert_eq!(
            project_request_key(&dot_alias.to_string_lossy()),
            canonical.to_string_lossy()
        );
        #[cfg(unix)]
        {
            let alias_parent = tempfile::tempdir().unwrap();
            let alias = alias_parent.path().join("project-link");
            std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
            assert_eq!(
                project_request_key(&alias.to_string_lossy()),
                canonical.to_string_lossy()
            );
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(project_request_key("demo"), "demo");
    }

    #[test]
    fn automatic_conversation_reads_use_the_verified_daemon_without_local_files() {
        use crate::reservation_read::tests::{TestReply, mailbox_payload, with_test_server};

        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(thread()),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(mailbox_payload(config)),
                ]
            },
            |config| {
                try_read_thread(
                    config,
                    &ProjectSelection::Exact("/work/demo".into()),
                    "TKT-1",
                    2,
                    None,
                    true,
                )
            },
        );
        let (project, result) = result.unwrap().expect("matching daemon selected");
        assert_eq!(project.slug, "demo");
        assert_eq!(result.message_count, 4);
        assert_eq!(result.messages[0].id, 3);
        assert!(
            requests[2]["params"]["uri"]
                .as_str()
                .unwrap()
                .contains("context=true")
        );
        assert_eq!(requests.len(), 5);

        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(single()),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(mailbox_payload(config)),
                ]
            },
            |config| try_read_message(config, &ProjectSelection::Exact("demo".into()), 3),
        );
        let (_, result) = result.unwrap().expect("matching daemon selected");
        assert_eq!(result.position, 3);
        assert_eq!(result.previous.unwrap().id, 2);
        assert_eq!(result.next.unwrap().id, 4);
        assert_eq!(requests.len(), 5);
    }

    #[test]
    fn automatic_conversation_reads_refuse_a_mailbox_swap_after_the_snapshot() {
        use crate::reservation_read::tests::{TestReply, mailbox_payload, with_test_server};

        let (result, _) = with_test_server(
            |config| {
                let mut swapped = mailbox_payload(config);
                swapped["storage_root"] = json!("/another/archive");
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(single()),
                    TestReply::Resource(project_resource()),
                    TestReply::Resource(swapped),
                ]
            },
            |config| try_read_message(config, &ProjectSelection::Exact("demo".into()), 3),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("mailbox identity changed")
        );
    }

    #[test]
    fn automatic_conversation_reads_preserve_offline_and_explicit_server_semantics() {
        use crate::reservation_read::tests::{TestReply, with_test_server};

        let (automatic, _) = with_test_server(
            |_| vec![TestReply::Raw(503, json!({"error":"unavailable"}))],
            |config| try_read_message(config, &ProjectSelection::Exact("demo".into()), 3),
        );
        assert!(automatic.unwrap().is_none());
        let (explicit, _) = with_test_server(
            |_| vec![TestReply::Raw(503, json!({"error":"unavailable"}))],
            |config| read_message(config, "demo", 3),
        );
        assert!(explicit.is_err(), "explicit --server never falls back");
    }

    #[test]
    fn implicit_nested_directory_reads_use_the_negotiated_daemon_ancestor() {
        use crate::reservation_read::tests::{
            TestReply, ancestor_mailbox_payload, with_test_server,
        };

        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("src").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let human_key = project_request_key(&directory.path().to_string_lossy());
        let requested_path = project_request_key(&nested.to_string_lossy());
        let selection = ProjectSelection::Ancestors(requested_path.clone());
        let mut descriptor = project_resource();
        descriptor["human_key"] = json!(human_key);
        descriptor["resolution"] =
            json!({"kind":"nearest_ancestor","requested_path":requested_path});
        let mut thread_snapshot = thread();
        thread_snapshot["project"]["human_key"] = json!(human_key);

        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                    TestReply::Resource(descriptor.clone()),
                    TestReply::Resource(thread_snapshot),
                    TestReply::Resource(descriptor.clone()),
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                ]
            },
            |config| try_read_thread(config, &selection, "TKT-1", 2, None, true),
        );
        let (project, snapshot) = result
            .unwrap()
            .expect("daemon selected registered ancestor");
        assert_eq!(project.human_key, human_key);
        assert_eq!(snapshot.message_count, 4);
        assert_eq!(
            requests[1]["params"]["uri"],
            format!(
                "resource://project/{}?resolve=ancestors&format=json",
                encode_resource_component(&requested_path)
            )
        );
        assert_eq!(requests[1]["params"]["uri"], requests[3]["params"]["uri"]);

        let mut message_snapshot = single();
        message_snapshot["project"]["human_key"] = json!(human_key);
        let (result, _) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                    TestReply::Resource(descriptor.clone()),
                    TestReply::Resource(message_snapshot),
                    TestReply::Resource(descriptor.clone()),
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                ]
            },
            |config| try_read_message(config, &selection, 3),
        );
        let (project, snapshot) = result
            .unwrap()
            .expect("daemon selected registered ancestor");
        assert_eq!(project.human_key, human_key);
        assert_eq!(snapshot.message.id, 3);
        assert_eq!(snapshot.position, 3);
    }

    #[cfg(unix)]
    #[test]
    fn implicit_directory_falls_back_only_before_ancestor_authority_is_accepted() {
        use crate::reservation_read::tests::{
            TestReply, ancestor_mailbox_payload, mailbox_payload, with_test_server,
        };
        let selection = ProjectSelection::Ancestors("/work/demo/src".into());
        for capability in [None, Some("future_unsupported_version")] {
            let (result, requests) = with_test_server(
                |config| {
                    let mut payload = mailbox_payload(config);
                    if let Some(capability) = capability {
                        payload["project_path_resolution"] = capability.into();
                    }
                    vec![TestReply::Resource(payload)]
                },
                |config| try_read_message(config, &selection, 3),
            );
            assert!(result.unwrap().is_none());
            assert_eq!(
                requests.len(),
                1,
                "unsupported mode must not issue an exact project read"
            );
        }
        let (result, _) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                    TestReply::Resource(project_resource()),
                ]
            },
            |config| try_read_message(config, &selection, 3),
        );
        assert!(result.unwrap_err().to_string().contains("missing ancestor"));
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_witness_must_bind_request_and_component_wise_parent_path() {
        let selection = ProjectSelection::Ancestors("/work/demo/src/nested".into());
        let requested = project_request_key(selection.key());
        let mut valid = project_resource();
        valid["resolution"] = json!({"kind":"nearest_ancestor","requested_path":requested});
        assert!(validate_project_selection(&valid, &selection).is_ok());
        for (field, value) in [
            ("kind", json!("exact")),
            ("requested_path", json!("/work/another/src")),
        ] {
            let mut invalid = valid.clone();
            invalid["resolution"][field] = value;
            assert!(validate_project_selection(&invalid, &selection).is_err());
        }
        for path in [
            "/work/demo/s",
            "/work/demo/src-other",
            "/work/unrelated",
            "relative",
        ] {
            let mut invalid = valid.clone();
            invalid["human_key"] = path.into();
            assert!(
                validate_project_selection(&invalid, &selection).is_err(),
                "{path}"
            );
        }
        assert!(
            validate_project_selection(&valid, &ProjectSelection::Exact("demo".into())).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_selection_refuses_a_different_nearest_project_after_the_read() {
        use crate::reservation_read::tests::{
            TestReply, ancestor_mailbox_payload, with_test_server,
        };
        let selection = ProjectSelection::Ancestors("/work/demo/src/nested".into());
        let mut before = project_resource();
        before["resolution"] = json!({"kind":"nearest_ancestor","requested_path":project_request_key(selection.key())});
        let mut after = before.clone();
        after["id"] = json!(8);
        after["slug"] = json!("nested-project");
        after["human_key"] = json!("/work/demo/src");
        let (result, _) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                    TestReply::Resource(before),
                    TestReply::Resource(single()),
                    TestReply::Resource(after),
                ]
            },
            |config| try_read_message(config, &selection, 3),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("project identity changed")
        );
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
