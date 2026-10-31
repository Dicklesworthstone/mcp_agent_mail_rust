//! Authenticated reservation reads through the running MCP server.
//!
//! A fresh CLI database connection can be refused while the server's admitted
//! pool remains healthy (GH#368). Automatic reads verify the configured local
//! mailbox; explicit server reads select their own authority. This module never
//! opens a database, repairs a mailbox, or guesses from an archive. Project
//! identity and the complete response are checked before returning reservations.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use mcp_agent_mail_core::Config;
use serde_json::Value;

use crate::context::ResolvedProject;
use crate::message_read::{ProjectSelection, project_resource_uri, read_project_descriptor};
use crate::{CliError, CliResult};

// The opt-in snapshot resource proves completeness in one database read scope.
// Its bounds are shared with the server; independent pages cannot prove this.
use mcp_agent_mail_db::queries::{
    FILE_RESERVATION_SNAPSHOT_MAX_BYTES, FILE_RESERVATION_SNAPSHOT_MAX_ROWS,
};
// Older running servers expose only capped resource pages. A short first page
// can prove completeness, but a full page cannot distinguish 250 from 251 rows.
const LEGACY_RESOURCE_PAGE_MAX: usize = 250;
const RESOURCE_TIMEOUT_SECONDS: u64 = 30;
const MAILBOX_PROBE_TIMEOUT_SECONDS: u64 = 2;
const MAILBOX_CONFIG_RESOURCE: &str = "resource://config/environment";

/// A pinned, authenticated resource authority shared by explicit server reads.
pub(crate) struct ServerResourceSession {
    client: asupersync::http::h1::HttpClient,
    server_url: String,
    bearer: Option<String>,
    ancestor_resolution: bool,
    inbox_priority_snapshot: bool,
}

impl ServerResourceSession {
    pub(crate) fn new(config: &Config) -> CliResult<Self> {
        let server_url = crate::local_server_url(config);
        let bearer = mcp_agent_mail_core::config::env_value("AGENT_MAIL_TOKEN")
            .filter(|token| !token.trim().is_empty())
            .or_else(|| crate::local_server_bearer_token(config))
            .map(|token| token.trim().to_string());
        if bearer.as_deref().is_some_and(|token| {
            token
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        }) {
            return Err(CliError::InvalidArgument(
                "server bearer token contains invalid whitespace or control characters".to_string(),
            ));
        }
        Ok(Self {
            client: asupersync::http::h1::HttpClient::builder()
                .no_redirects()
                .no_retries()
                .build(),
            server_url,
            bearer,
            ancestor_resolution: false,
            inbox_priority_snapshot: false,
        })
    }

    pub(crate) async fn read(&self, uri: &str) -> CliResult<Value> {
        read_resource_json(
            &self.client,
            &self.server_url,
            self.bearer.as_deref(),
            uri,
            RESOURCE_TIMEOUT_SECONDS,
        )
        .await
    }

    /// Select the daemon only when its authenticated configuration names the
    /// same SQLite/archive pair. This probe reads configuration, not health or
    /// database state, so ordinary CLI reads do not open a second mailbox or
    /// pay for an archive inventory. Only an unavailable endpoint, an older
    /// server without archive identity, or a different mailbox permits local
    /// fallback; authentication and malformed responses remain errors.
    pub(crate) async fn for_local_mailbox(config: &Config) -> CliResult<Option<Self>> {
        let mut session = Self::new(config)?;
        // Equal path strings on different machines do not establish mailbox
        // identity. Remote authority is selected explicitly with --server.
        if !is_local_mailbox_endpoint(&session.server_url)? {
            return Ok(None);
        }
        let id = format!("cli-resource-read-{MAILBOX_CONFIG_RESOURCE}");
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": &id,
            "method": "resources/read",
            "params": { "uri": MAILBOX_CONFIG_RESOURCE },
        });
        let response = match crate::post_jsonrpc_request_with_client(
            &session.server_url,
            session.bearer.as_deref(),
            &request,
            MAILBOX_PROBE_TIMEOUT_SECONDS,
            &session.client,
        )
        .await
        {
            Ok(response) => response,
            Err(error) if crate::server_tool_error_is_unavailable(&error) => return Ok(None),
            Err(error) => return Err(sanitize_transport_error(error)),
        };
        let payload = validate_resource_response(&response, &id, MAILBOX_CONFIG_RESOURCE)?;
        if !mailbox_config_matches(&payload, config)? {
            return Ok(None);
        }
        session.ancestor_resolution = supports_ancestor_resolution(&payload)?;
        session.inbox_priority_snapshot = supports_inbox_priority_snapshot(&payload)?;
        Ok(Some(session))
    }

    pub(crate) const fn supports_ancestor_resolution(&self) -> bool {
        self.ancestor_resolution
    }

    pub(crate) const fn supports_inbox_priority_snapshot(&self) -> bool {
        self.inbox_priority_snapshot
    }

    pub(crate) async fn verify_local_mailbox(&self, config: &Config) -> CliResult<()> {
        let payload = read_resource_json(
            &self.client,
            &self.server_url,
            self.bearer.as_deref(),
            MAILBOX_CONFIG_RESOURCE,
            MAILBOX_PROBE_TIMEOUT_SECONDS,
        )
        .await?;
        if !mailbox_config_matches(&payload, config)?
            || (self.ancestor_resolution && !supports_ancestor_resolution(&payload)?)
            || (self.inbox_priority_snapshot && !supports_inbox_priority_snapshot(&payload)?)
        {
            return Err(invalid_response(
                "mailbox identity changed during server read",
            ));
        }
        Ok(())
    }

    /// A tool read on the same pinned endpoint and credential as the resource
    /// identity checks. Never follow a redirect or reinterpret a rejection as
    /// permission to read a local mailbox.
    pub(crate) async fn call_tool(&self, tool: &str, arguments: Value) -> CliResult<Value> {
        self.call_tool_with_timeout(tool, arguments, RESOURCE_TIMEOUT_SECONDS)
            .await
    }

    pub(crate) async fn call_tool_with_timeout(
        &self,
        tool: &str,
        arguments: Value,
        timeout_seconds: u64,
    ) -> CliResult<Value> {
        let id = format!("cli-read-{tool}");
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": &id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let response = crate::post_jsonrpc_request_with_client(
            &self.server_url,
            self.bearer.as_deref(),
            &request,
            timeout_seconds,
            &self.client,
        )
        .await
        .map_err(sanitize_transport_error)?;
        if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || response.get("id").and_then(Value::as_str) != Some(id.as_str())
            || response.get("error").is_some()
        {
            return Err(invalid_response(
                "tool read was rejected or its request identity mismatched",
            ));
        }
        let result = response
            .get("result")
            .filter(|result| result.is_object())
            .ok_or_else(|| invalid_response("missing tool result object"))?;
        if result
            .get("isError")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            return Err(invalid_response("server rejected the tool read"));
        }
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .filter(|items| items.len() == 1)
            .ok_or_else(|| invalid_response("expected exactly one tool text content item"))?;
        if content[0].get("type").and_then(Value::as_str) != Some("text") {
            return Err(invalid_response("tool read did not return text content"));
        }
        let text = content[0]
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| text.len() <= FILE_RESERVATION_SNAPSHOT_MAX_BYTES)
            .ok_or_else(|| invalid_response("tool text is missing or exceeds the read limit"))?;
        let payload: Value = serde_json::from_str(text)
            .map_err(|_| invalid_response("tool text is not valid JSON"))?;
        for key in ["structuredContent", "structured_content"] {
            if result
                .get(key)
                .is_some_and(|structured| structured != &payload)
            {
                return Err(invalid_response(
                    "tool text and structured content disagree",
                ));
            }
        }
        Ok(payload)
    }
}

fn supports_ancestor_resolution(value: &Value) -> CliResult<bool> {
    match value.get("project_path_resolution") {
        None => Ok(false),
        Some(Value::String(capability)) => Ok(capability == "nearest_ancestor_v1"),
        Some(_) => Err(invalid_response(
            "invalid project path resolution capability",
        )),
    }
}

fn supports_inbox_priority_snapshot(value: &Value) -> CliResult<bool> {
    match value.get("inbox_priority_snapshot") {
        None => Ok(false),
        Some(Value::String(capability)) => Ok(capability == "priority_v1"),
        Some(_) => Err(invalid_response(
            "invalid inbox priority snapshot capability",
        )),
    }
}

fn is_local_mailbox_endpoint(endpoint: &str) -> CliResult<bool> {
    let parsed = asupersync::http::h1::ParsedUrl::parse(endpoint)
        .map_err(|_| invalid_response("invalid MCP endpoint URL"))?;
    let host = parsed.host.trim_start_matches('[').trim_end_matches(']');
    Ok(host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()))
}

pub(crate) fn mailbox_config_matches(value: &Value, config: &Config) -> CliResult<bool> {
    let database_url = nonempty_text(value, "database_url")?;
    // Older servers do not expose the archive root. They cannot authorize an
    // automatic switch from the selected local mailbox to this endpoint.
    if value.get("storage_root").is_none() {
        return Ok(false);
    }
    let storage_root = nonempty_text(value, "storage_root")?;
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url)
        || mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url)
    {
        return Ok(false);
    }
    // A relative path belongs to the daemon's working directory, which is not
    // established by this resource. Do not resolve it against the CLI's CWD.
    if !Path::new(storage_root).is_absolute()
        || mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(database_url)
            .is_none_or(|path| !path.is_absolute())
    {
        return Ok(false);
    }
    let database_identity = |url: &str| {
        mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(url)
            .map(|resolved| resolved.canonical_path)
    };
    let expected_database = database_identity(&config.database_url)
        .map_err(|_| invalid_response("selected mailbox database URL is invalid"))?;
    let actual_database = database_identity(database_url)
        .map_err(|_| invalid_response("server mailbox database URL is invalid"))?;
    let path_identity =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(expected_database == actual_database
        && path_identity(&config.storage_root) == path_identity(Path::new(storage_root)))
}

#[derive(Debug, Clone)]
pub(crate) struct ServerReservation {
    pub id: i64,
    pub agent: String,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: String,
    pub created_ts: i64,
    pub expires_ts: i64,
    pub released_ts: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct ServerReservationRead {
    pub project: ResolvedProject,
    pub agent_names: Vec<String>,
    pub reservations: Vec<ServerReservation>,
}

/// Read reservations without admitting a new connection to the local mailbox.
///
/// Always require a complete listing. Independent OFFSET requests are not a
/// consistent snapshot and may silently skip a live lease as other agents
/// release reservations. Native CLI ordering and pagination are applied only
/// after the complete response has been verified.
pub(crate) fn read_server(
    config: &Config,
    project_key: &str,
    active_only: bool,
) -> CliResult<ServerReservationRead> {
    let project_key = project_key.trim();
    if project_key.is_empty() {
        return Err(CliError::InvalidArgument(
            "server reservation reads require a project path or slug".to_string(),
        ));
    }

    // Pin the endpoint and credential for every read. In particular, never
    // retry an authentication rejection on a different endpoint or in SQLite.
    let session = ServerResourceSession::new(config)?;
    let selection = ProjectSelection::Exact(project_key.to_string());
    // An explicit remote authority may use different filesystem aliases.
    // Preserve its original exact selector instead of canonicalizing locally.
    let project_uri = format!(
        "resource://project/{}?format=json",
        encode_resource_component(project_key)
    );
    crate::context::run_async(read_reservations_session(
        &session,
        &selection,
        &project_uri,
        active_only,
    ))
}

/// Prefer a matching local daemon without opening a second mailbox. Only
/// unsupported scope or unavailable/unmatched authority allows local fallback.
pub(crate) fn try_read_local_mailbox(
    config: &Config,
    selection: &ProjectSelection,
    active_only: bool,
) -> CliResult<Option<ServerReservationRead>> {
    if !selection.permits_automatic_read(config) {
        return Ok(None);
    }
    let project_uri = project_resource_uri(selection)?;
    crate::context::run_async(async {
        let Some(session) = ServerResourceSession::for_local_mailbox(config).await? else {
            return Ok(None);
        };
        if !selection.supported_by(&session) {
            return Ok(None);
        }
        let result =
            read_reservations_session(&session, selection, &project_uri, active_only).await?;
        session.verify_local_mailbox(config).await?;
        Ok(Some(result))
    })
}

async fn read_reservations_session(
    session: &ServerResourceSession,
    selection: &ProjectSelection,
    project_uri: &str,
    active_only: bool,
) -> CliResult<ServerReservationRead> {
    let (project, before) = read_project_descriptor(session, project_uri, selection).await?;
    let agent_roster = validate_agent_roster(&before, project.id)?;

    let reservation_uri = format!(
        "resource://file_reservations/{}?snapshot=true&active_only={active_only}&format=json",
        encode_resource_component(&project.slug)
    );
    let payload = session.read(&reservation_uri).await?;
    let (holder_roster, reservations) = if payload.is_array() {
        // v0.3.39 ignores the unknown snapshot parameter and returns its
        // ordinary default page (20 rows). That page is only a protocol
        // signal, never evidence of a complete listing. Validate it before
        // negotiating, then discard it and request the explicit page bound
        // from the same authenticated authority. Do not negotiate after an
        // HTTP/RPC error, invalid envelope, or malformed snapshot object.
        validate_legacy_reservation_page(&payload, active_only)?;
        let legacy_uri = format!(
            "resource://file_reservations/{}?active_only={active_only}&limit={LEGACY_RESOURCE_PAGE_MAX}&offset=0&format=json",
            encode_resource_component(&project.slug)
        );
        let legacy_payload = session.read(&legacy_uri).await?;
        let reservations = validate_complete_legacy_reservations(&legacy_payload, active_only)?;
        // Legacy pages identify holders by name, not ID. Preserve those
        // names in the rows, while explicit --agent resolution continues
        // to use the independently verified available-agent directory.
        (agent_roster.clone(), reservations)
    } else {
        validate_snapshot(&payload, &project, active_only)?
    };
    if agent_roster
        .iter()
        .any(|agent| holder_roster.binary_search(agent).is_err())
    {
        return Err(invalid_response(
            "snapshot omits a registered project identity",
        ));
    }

    // Revalidate the stored project (and ancestor witness, when selected),
    // creation epoch, and roster so recreation or a newly nearer project
    // cannot lend authority to a stale reservation response.
    let (after_project, after) = read_project_descriptor(session, project_uri, selection).await?;
    let after_agent_roster = validate_agent_roster(&after, after_project.id)?;
    if !same_project(&project, &after_project) || agent_roster != after_agent_roster {
        return Err(CliError::Other(
            "server project identity or agent roster changed during reservation read; refusing unverified reservation data"
                .to_string(),
        ));
    }

    Ok(ServerReservationRead {
        project,
        agent_names: holder_roster.into_iter().map(|(_, name)| name).collect(),
        reservations,
    })
}

pub(crate) fn encode_resource_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

async fn read_resource_json(
    client: &asupersync::http::h1::HttpClient,
    server_url: &str,
    bearer: Option<&str>,
    uri: &str,
    timeout_seconds: u64,
) -> CliResult<Value> {
    let id = format!("cli-resource-read-{uri}");
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": &id,
        "method": "resources/read",
        "params": { "uri": uri },
    });
    let response = crate::post_jsonrpc_request_with_client(
        server_url,
        bearer,
        &request,
        timeout_seconds,
        client,
    )
    .await
    .map_err(sanitize_transport_error)?;
    validate_resource_response(&response, &id, uri)
}

fn sanitize_transport_error(error: CliError) -> CliError {
    // The generic transport annotates errors with the endpoint, and a malformed
    // HTTP status line can include peer text. Neither is safe to repeat here:
    // URLs can contain credentials and peer text may reflect our bearer token.
    let message = error.to_string();
    if message.starts_with("authentication failed") {
        return CliError::Other(
            "server resource read authentication failed; check AGENT_MAIL_TOKEN/HTTP_BEARER_TOKEN"
                .to_string(),
        );
    }
    if let Some(status) = message
        .strip_prefix("unexpected HTTP status ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|status| status.parse::<u16>().ok())
    {
        return CliError::Other(if (300..400).contains(&status) {
            format!(
                "server resource read refused an HTTP redirect ({status}); configure the final trusted AGENT_MAIL_URL explicitly"
            )
        } else {
            format!("server resource read failed with HTTP status {status}")
        });
    }
    if matches!(error, CliError::Io(_))
        || message.starts_with("transport failure calling ")
        || message.starts_with("request to ")
    {
        return CliError::Other(
            "server resource unavailable or timed out; check AGENT_MAIL_URL/HTTP_HOST/HTTP_PORT/HTTP_PATH"
                .to_string(),
        );
    }
    CliError::Other(
        "server resource read failed because the endpoint configuration or HTTP/JSON response was invalid; no data accepted"
            .to_string(),
    )
}

fn invalid_response(detail: &str) -> CliError {
    CliError::Other(format!(
        "invalid server resource response: {detail}; refusing unverified data"
    ))
}

/// Validate the transport envelope before interpreting its resource contents.
/// Never include a peer-supplied body or error message in diagnostics: either
/// may contain credentials or unrelated mailbox data.
fn validate_resource_response(response: &Value, id: &str, uri: &str) -> CliResult<Value> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || response.get("id").and_then(Value::as_str) != Some(id)
    {
        return Err(invalid_response("JSON-RPC version or request id mismatch"));
    }
    if response.get("error").is_some() {
        if response.get("result").is_some() {
            return Err(invalid_response(
                "both JSON-RPC result and error are present",
            ));
        }
        let code = response
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64);
        return Err(CliError::Other(match code {
            Some(code) => format!(
                "server rejected resource read (JSON-RPC error {code}); verify the project and server authentication"
            ),
            None => "server rejected resource read with a malformed JSON-RPC error".to_string(),
        }));
    }
    let result = response
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("missing JSON-RPC result object"))?;
    if result.get("isError").is_some() {
        return Err(invalid_response("tool result returned for resources/read"));
    }
    let contents = result
        .get("contents")
        .and_then(Value::as_array)
        .filter(|contents| contents.len() == 1)
        .ok_or_else(|| invalid_response("expected exactly one resource content item"))?;
    let content = &contents[0];
    if content.get("uri").and_then(Value::as_str) != Some(uri) {
        return Err(invalid_response("resource URI mismatch"));
    }
    if content.get("blob").is_some() {
        return Err(invalid_response(
            "binary content returned for JSON resource",
        ));
    }
    let text = content
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("missing resource text"))?;
    if text.len() > FILE_RESERVATION_SNAPSHOT_MAX_BYTES {
        return Err(invalid_response(
            "resource text exceeds the 4 MiB snapshot limit",
        ));
    }
    serde_json::from_str(text).map_err(|_| invalid_response("resource text is not valid JSON"))
}

fn required_text<'a>(value: &'a Value, key: &str) -> CliResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response(&format!("missing or non-string {key}")))
}

fn nonempty_text<'a>(value: &'a Value, key: &str) -> CliResult<&'a str> {
    let text = required_text(value, key)?;
    if text.trim().is_empty() {
        return Err(invalid_response(&format!("empty {key}")));
    }
    Ok(text)
}

fn positive_id(value: &Value) -> CliResult<i64> {
    value
        .get("id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| invalid_response("missing or invalid positive id"))
}

fn timestamp(value: &Value, key: &str) -> CliResult<i64> {
    mcp_agent_mail_core::timestamps::iso_to_micros(required_text(value, key)?)
        .ok_or_else(|| invalid_response(&format!("invalid {key} timestamp")))
}

pub(crate) fn validate_project(value: &Value, requested_key: &str) -> CliResult<ResolvedProject> {
    let project = ResolvedProject {
        id: positive_id(value)?,
        slug: nonempty_text(value, "slug")?.to_string(),
        human_key: nonempty_text(value, "human_key")?.to_string(),
        created_at: timestamp(value, "created_at")?,
    };
    verify_project_selector(&project, requested_key)?;
    Ok(project)
}

/// Verify an additional CLI or resource selector against the stored project.
/// Every explicit selector must agree; a URI slug must not override a
/// conflicting `--project` path or query selector.
pub(crate) fn verify_project_selector(project: &ResolvedProject, selector: &str) -> CliResult<()> {
    let requested_key = selector.trim();
    let matches = if Path::new(requested_key).is_absolute() {
        // Compare the actual directory identity without Git identity discovery:
        // resolve_project_identity can create .git/agent-mail/project-id under
        // worktree identity modes, which a reservation read must never do.
        // Exact paths remain valid when the project is not mounted locally.
        if Path::new(&project.human_key).is_absolute() {
            requested_key == project.human_key
                || std::fs::canonicalize(requested_key)
                    .ok()
                    .zip(std::fs::canonicalize(&project.human_key).ok())
                    .is_some_and(|(requested, actual)| requested == actual)
        } else {
            false
        }
    } else {
        project.slug.eq_ignore_ascii_case(requested_key) || project.human_key == requested_key
    };
    if !matches {
        return Err(CliError::Other(
            "server project identity does not match the requested project; refusing unverified data"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn same_project(left: &ResolvedProject, right: &ResolvedProject) -> bool {
    left.id == right.id
        && left.slug == right.slug
        && left.human_key == right.human_key
        && left.created_at == right.created_at
}

type AgentRoster = Vec<(i64, String)>;

fn validate_agent_roster(value: &Value, project_id: i64) -> CliResult<AgentRoster> {
    validate_roster(value, project_id, false)
}

fn validate_roster(
    value: &Value,
    project_id: i64,
    allow_legacy_ids: bool,
) -> CliResult<AgentRoster> {
    // Unlike the reservation resource, project details returns its entire
    // registered-agent roster. Validate this authority separately from lease
    // holders: an agent with no reservations is still a valid --agent scope.
    let agents = value
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("missing or invalid project agent roster"))?;
    let mut ids = HashSet::with_capacity(agents.len());
    let mut roster = Vec::with_capacity(agents.len());
    for agent in agents {
        if agent.get("project_id").and_then(Value::as_i64) != Some(project_id) {
            return Err(invalid_response(
                "agent roster belongs to a different project",
            ));
        }
        let id = if allow_legacy_ids {
            agent
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid_response("missing or invalid holder id"))?
        } else {
            positive_id(agent)?
        };
        if !ids.insert(id) {
            return Err(invalid_response("duplicate agent id in project roster"));
        }
        roster.push((id, nonempty_text(agent, "name")?.to_string()));
    }
    roster.sort();
    Ok(roster)
}

fn validate_reservations(value: &Value, active_only: bool) -> CliResult<Vec<ServerReservation>> {
    let rows = value
        .as_array()
        .ok_or_else(|| invalid_response("reservation content is not an array"))?;
    if rows.len() > FILE_RESERVATION_SNAPSHOT_MAX_ROWS {
        return Err(invalid_response(
            "reservation snapshot exceeds the row limit",
        ));
    }
    let mut ids = HashSet::with_capacity(rows.len());
    let mut reservations = Vec::with_capacity(rows.len());
    for row in rows {
        let id = positive_id(row)?;
        if !ids.insert(id) {
            return Err(invalid_response("duplicate reservation id"));
        }
        let released_ts = match row.get("released_ts") {
            Some(Value::Null) => None,
            Some(Value::String(_)) => Some(timestamp(row, "released_ts")?),
            _ => return Err(invalid_response("missing or invalid released_ts")),
        };
        if active_only && released_ts.is_some_and(|ts| ts > 0) {
            return Err(invalid_response(
                "released reservation in active-only response",
            ));
        }
        let exclusive = row
            .get("exclusive")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid_response("missing or non-boolean exclusive flag"))?;
        let reservation = ServerReservation {
            id,
            agent: nonempty_text(row, "agent")?.to_string(),
            path_pattern: nonempty_text(row, "path_pattern")?.to_string(),
            exclusive,
            reason: required_text(row, "reason")?.to_string(),
            created_ts: timestamp(row, "created_ts")?,
            expires_ts: timestamp(row, "expires_ts")?,
            released_ts,
        };
        if reservation.expires_ts < reservation.created_ts {
            return Err(invalid_response("reservation expires before its creation"));
        }
        reservations.push(reservation);
    }
    Ok(reservations)
}

fn validate_legacy_reservation_page(
    value: &Value,
    active_only: bool,
) -> CliResult<Vec<ServerReservation>> {
    let rows = value
        .as_array()
        .ok_or_else(|| invalid_response("legacy reservation content is not an array"))?;
    if rows.len() > LEGACY_RESOURCE_PAGE_MAX {
        return Err(invalid_response(
            "legacy reservation response exceeds the requested page",
        ));
    }
    validate_reservations(value, active_only)
}

fn validate_complete_legacy_reservations(
    value: &Value,
    active_only: bool,
) -> CliResult<Vec<ServerReservation>> {
    let reservations = validate_legacy_reservation_page(value, active_only)?;
    if reservations.len() == LEGACY_RESOURCE_PAGE_MAX {
        return Err(CliError::Other(format!(
            "server returned a full {LEGACY_RESOURCE_PAGE_MAX}-row legacy reservation page; \
             a complete reservation list cannot be verified. A successful direct CLI read \
             or a server with complete snapshot support is required; independent resource \
             pages are not a complete snapshot"
        )));
    }
    Ok(reservations)
}

fn validate_snapshot(
    value: &Value,
    project: &ResolvedProject,
    active_only: bool,
) -> CliResult<(AgentRoster, Vec<ServerReservation>)> {
    if value.get("schema_version").and_then(Value::as_u64) != Some(1)
        || value.get("complete").and_then(Value::as_bool) != Some(true)
        || value.get("active_only").and_then(Value::as_bool) != Some(active_only)
    {
        return Err(invalid_response(
            "server did not return a valid complete reservation snapshot",
        ));
    }
    let snapshot_project = validate_project(
        value
            .get("project")
            .ok_or_else(|| invalid_response("missing snapshot project"))?,
        &project.slug,
    )?;
    if !same_project(project, &snapshot_project) {
        return Err(invalid_response("snapshot project identity mismatch"));
    }
    let captured_ts = timestamp(value, "captured_ts")?;
    let agents = value
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("missing snapshot holder identities"))?;
    if agents.len() > FILE_RESERVATION_SNAPSHOT_MAX_ROWS {
        return Err(invalid_response(
            "snapshot holder count exceeds the row limit",
        ));
    }
    // The project directory advertises active recipients; this authority also
    // includes retired holders so historical leases keep their original names.
    let roster = validate_roster(value, project.id, true)?;
    let rows = value
        .get("reservations")
        .ok_or_else(|| invalid_response("missing snapshot reservations"))?;
    let reservations = validate_reservations(rows, active_only)?;
    if value.get("count").and_then(Value::as_u64) != u64::try_from(reservations.len()).ok() {
        return Err(invalid_response(
            "snapshot count does not match its complete row set",
        ));
    }
    let holders: std::collections::HashMap<_, _> = roster
        .iter()
        .map(|(id, name)| (*id, name.as_str()))
        .collect();
    for (row, reservation) in rows
        .as_array()
        .expect("validated reservation array")
        .iter()
        .zip(&reservations)
    {
        if row.get("project_id").and_then(Value::as_i64) != Some(project.id) {
            return Err(invalid_response(
                "reservation belongs to a different project",
            ));
        }
        let holder = row
            .get("agent_id")
            .and_then(Value::as_i64)
            .and_then(|id| holders.get(&id));
        if holder.copied() != Some(reservation.agent.as_str()) {
            return Err(invalid_response("reservation holder identity mismatch"));
        }
        if active_only && reservation.expires_ts <= captured_ts {
            return Err(invalid_response(
                "expired reservation in active-only snapshot",
            ));
        }
    }
    Ok((roster, reservations))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) enum TestReply {
        Resource(Value),
        Tool(Value),
        Raw(u16, Value),
    }

    /// Exercise the real HTTP and JSON-RPC client without requiring a local
    /// mailbox. Scripted peer data challenges routing and trust decisions;
    /// the complete conversation resource has separate real DB coverage.
    pub(crate) fn with_test_server<R>(
        replies: impl FnOnce(&Config) -> Vec<TestReply>,
        run: impl FnOnce(&Config) -> R,
    ) -> (R, Vec<Value>) {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &directory.path().join("never-opened.sqlite3"),
            ),
            storage_root: directory.path().join("never-opened-archive"),
            http_host: "127.0.0.1".into(),
            http_port: port,
            http_path: "/mcp/".into(),
            http_bearer_token: Some("read-test-token".into()),
            ..Config::default()
        };
        let replies = replies(&config);
        let endpoint = format!("http://127.0.0.1:{port}/mcp/");
        let outcome = std::thread::scope(|scope| {
            let peer = scope.spawn(move || {
                let mut requests = Vec::new();
                for reply in replies {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let (mut stream, _) = loop {
                        match listener.accept() {
                            Ok(connection) => break connection,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "missing scripted request");
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("accept failed: {error}"),
                        }
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut reader = std::io::BufReader::new(&mut stream);
                    let mut length = None;
                    let mut authenticated = false;
                    loop {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = Some(value.trim().parse::<usize>().unwrap());
                            }
                            if name.eq_ignore_ascii_case("authorization") {
                                authenticated = value.trim() == "Bearer read-test-token";
                            }
                        }
                    }
                    assert!(authenticated, "the pinned credential must be sent");
                    let mut bytes = vec![0; length.unwrap()];
                    reader.read_exact(&mut bytes).unwrap();
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    let (status, body) = match reply {
                        TestReply::Resource(payload) => {
                            assert_eq!(request["method"], "resources/read");
                            (200, serde_json::json!({
                                "jsonrpc":"2.0", "id":request["id"],
                                "result":{"contents":[{"uri":request["params"]["uri"],
                                    "mimeType":"application/json", "text":payload.to_string()}]}
                            }))
                        }
                        TestReply::Tool(payload) => {
                            assert_eq!(request["method"], "tools/call");
                            (200, serde_json::json!({
                                "jsonrpc":"2.0", "id":request["id"],
                                "result":{"isError":false,"content":[{"type":"text","text":payload.to_string()}]}
                            }))
                        }
                        TestReply::Raw(status, mut payload) => {
                            if let Some(object) = payload.as_object_mut() {
                                object.entry("id").or_insert_with(|| request["id"].clone());
                            }
                            (status, payload)
                        }
                    };
                    let body = body.to_string();
                    write!(stream,
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                    ).unwrap();
                    requests.push(request);
                }
                requests
            });
            let result = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("AGENT_MAIL_URL", &endpoint),
                    ("AGENT_MAIL_TOKEN", "read-test-token"),
                    ("DATABASE_URL", &config.database_url),
                    ("STORAGE_ROOT", config.storage_root.to_str().unwrap()),
                    ("WORKTREES_ENABLED", "false"),
                ],
                || run(&config),
            );
            (result, peer.join().unwrap())
        });
        assert!(
            !config.storage_root.exists(),
            "routing must not initialize an archive"
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "routing must not initialize a DB, sidecar or lock"
        );
        outcome
    }

    pub(crate) fn mailbox_payload(config: &Config) -> Value {
        serde_json::json!({"database_url": config.database_url, "storage_root": config.storage_root})
    }

    pub(crate) fn ancestor_mailbox_payload(config: &Config) -> Value {
        let mut payload = mailbox_payload(config);
        payload["project_path_resolution"] = "nearest_ancestor_v1".into();
        payload
    }

    #[test]
    fn automatic_mailbox_identity_never_equates_paths_on_different_hosts() {
        for endpoint in [
            "http://localhost:8765/mcp/",
            "http://127.0.0.1:8765/mcp/",
            "https://[::1]/mcp/",
        ] {
            assert!(is_local_mailbox_endpoint(endpoint).unwrap(), "{endpoint}");
        }
        for endpoint in [
            "https://mail.example/mcp/",
            "http://192.0.2.1/mcp/",
            "http://localhost.example/mcp/",
        ] {
            assert!(!is_local_mailbox_endpoint(endpoint).unwrap(), "{endpoint}");
        }
        assert!(is_local_mailbox_endpoint("http://localhost@evil.example/mcp/").is_err());
    }

    #[test]
    fn tool_reads_refuse_malformed_or_contradictory_success_envelopes() {
        for result in [
            serde_json::json!({"isError":"false","content":[{"type":"text","text":"{}"}]}),
            serde_json::json!({"isError":true,"content":[{"type":"text","text":"{}"}]}),
            serde_json::json!({"content":[{"type":"image","text":"{}"}]}),
            serde_json::json!({"content":[{"type":"text","text":"{}"}],"structuredContent":{"different":true}}),
            serde_json::json!({"content":[]}),
        ] {
            let (result, _) = with_test_server(
                |config| {
                    vec![
                        TestReply::Resource(mailbox_payload(config)),
                        TestReply::Raw(200, serde_json::json!({"jsonrpc":"2.0","result":result})),
                    ]
                },
                |config| {
                    crate::context::run_async(async {
                        let session = ServerResourceSession::for_local_mailbox(config)
                            .await?
                            .unwrap();
                        session
                            .call_tool("search_messages", serde_json::json!({}))
                            .await
                    })
                },
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn health_probe_pins_the_mailbox_without_opening_local_storage() {
        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Tool(mailbox_payload(config)),
                    TestReply::Resource(mailbox_payload(config)),
                ]
            },
            crate::probe_local_jsonrpc_health_with_latency,
        );
        let (payload, latency) = result.unwrap().expect("verified daemon health");
        assert!(payload["database_url"].is_string());
        assert!(latency >= 0.0);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["params"]["uri"], MAILBOX_CONFIG_RESOURCE);
        assert_eq!(requests[1]["params"]["name"], "health_check");
        assert_eq!(requests[2]["params"]["uri"], MAILBOX_CONFIG_RESOURCE);

        // Doctor explicitly adopts the daemon's advertised mailbox when its
        // default XDG mailbox differs. The health probe must use that target.
        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Tool(mailbox_payload(config)),
                    TestReply::Resource(mailbox_payload(config)),
                ]
            },
            |config| {
                let mut cli_config = config.clone();
                cli_config.database_url = "sqlite:////different/xdg/default.db".into();
                cli_config.storage_root = "/different/xdg/archive".into();
                crate::robot::live_server_proves_mailbox_healthy(
                    &cli_config,
                    &config.database_url,
                    &config.storage_root,
                )
            },
        );
        // Minimal peer data proves routing, never a healthy database verdict
        // or permission to silently fall back after a malformed health reply.
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("incomplete health evidence")
        );
        assert_eq!(requests.len(), 3, "doctor must probe its selected mailbox");
    }

    #[test]
    fn health_probe_refuses_rejections_malformed_replies_and_identity_drift() {
        for status in [401, 403, 307] {
            let (result, requests) = with_test_server(
                |_| {
                    vec![TestReply::Raw(
                        status,
                        serde_json::json!({"error":"private-token"}),
                    )]
                },
                crate::probe_local_jsonrpc_health_with_latency,
            );
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("private-token"));
            assert_eq!(requests.len(), 1);
        }
        for response in [
            serde_json::json!({"jsonrpc":"2.0","id":"wrong-request","result":{}}),
            serde_json::json!({"jsonrpc":"1.0","result":{}}),
            serde_json::json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"private-token"}}),
            serde_json::json!({"jsonrpc":"2.0","result":{"isError":true,"content":[{"type":"text","text":"{}"}]}}),
            serde_json::json!({"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{}"}],"structuredContent":{"different":true}}}),
            serde_json::json!({"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"x".repeat(FILE_RESERVATION_SNAPSHOT_MAX_BYTES + 1)}]}}),
        ] {
            let (result, requests) = with_test_server(
                |config| {
                    vec![
                        TestReply::Resource(mailbox_payload(config)),
                        TestReply::Raw(200, response),
                    ]
                },
                crate::probe_local_jsonrpc_health_with_latency,
            );
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("private-token"));
            assert_eq!(requests.len(), 2);
        }
        for drift_in_health in [true, false] {
            let (result, requests) = with_test_server(
                |config| {
                    let mut other = mailbox_payload(config);
                    other["database_url"] = "sqlite:////another/mailbox.db".into();
                    let mut replies = vec![TestReply::Resource(mailbox_payload(config))];
                    if drift_in_health {
                        replies.push(TestReply::Tool(other));
                    } else {
                        replies.push(TestReply::Tool(mailbox_payload(config)));
                        replies.push(TestReply::Resource(other));
                    }
                    replies
                },
                crate::probe_local_jsonrpc_health_with_latency,
            );
            assert!(result.is_err(), "changed mailbox must not permit fallback");
            assert_eq!(requests.len(), if drift_in_health { 2 } else { 3 });
        }

        let (result, requests) = with_test_server(
            |config| {
                let mut other = mailbox_payload(config);
                other["storage_root"] = "/other/mailbox".into();
                vec![TestReply::Resource(other)]
            },
            crate::probe_local_jsonrpc_health_with_latency,
        );
        assert!(
            result.unwrap().is_none(),
            "unselected daemon permits local reads"
        );
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn automatic_mailbox_selection_requires_both_file_authorities() {
        let (result, requests) = with_test_server(
            |config| vec![TestReply::Resource(mailbox_payload(config))],
            |config| {
                crate::context::run_async(ServerResourceSession::for_local_mailbox(config))
                    .map(|session| session.is_some())
            },
        );
        assert!(result.unwrap());
        assert_eq!(requests[0]["params"]["uri"], MAILBOX_CONFIG_RESOURCE);

        for variant in ["database", "archive", "legacy", "relative", "memory"] {
            let (result, _) = with_test_server(
                |config| {
                    let mut payload = mailbox_payload(config);
                    match variant {
                        "database" => {
                            payload["database_url"] = "sqlite:////unrelated/mail.db".into()
                        }
                        "archive" => payload["storage_root"] = "/unrelated/archive".into(),
                        "legacy" => {
                            payload.as_object_mut().unwrap().remove("storage_root");
                        }
                        "relative" => payload["database_url"] = "sqlite:///relative.db".into(),
                        "memory" => payload["database_url"] = "sqlite:///:memory:".into(),
                        _ => unreachable!(),
                    }
                    vec![TestReply::Resource(payload)]
                },
                |config| {
                    crate::context::run_async(ServerResourceSession::for_local_mailbox(config))
                        .map(|session| session.is_none())
                },
            );
            assert!(result.unwrap(), "{variant} must not select the daemon");
        }
    }

    #[test]
    fn automatic_mailbox_selection_does_not_fall_back_after_rejection_or_bad_identity() {
        for status in [200, 401, 403, 307] {
            let (result, _) = with_test_server(
                |_| {
                    vec![TestReply::Raw(
                        status,
                        serde_json::json!({
                            "jsonrpc":"2.0", "error":{"code":-32001,"message":"private-token"}
                        }),
                    )]
                },
                |config| {
                    crate::context::run_async(ServerResourceSession::for_local_mailbox(config))
                        .map(|session| session.is_some())
                },
            );
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("private-token"));
        }
        let (result, _) = with_test_server(
            |config| {
                let mut payload = mailbox_payload(config);
                payload["storage_root"] = Value::Null;
                vec![TestReply::Resource(payload)]
            },
            |config| {
                crate::context::run_async(ServerResourceSession::for_local_mailbox(config))
                    .map(|session| session.is_some())
            },
        );
        assert!(result.is_err());
    }

    fn project() -> Value {
        serde_json::json!({
            "id": 7,
            "slug": "test-project",
            "human_key": "/tmp/agent-mail-reservation-reader-project",
            "created_at": "2026-10-26T01:00:00Z",
            "agents": [],
        })
    }

    fn reservation(id: i64) -> Value {
        serde_json::json!({
            "id": id,
            "project_id": 7,
            "agent_id": 12,
            "agent": "BlueLake",
            "path_pattern": "src/**",
            "exclusive": true,
            "reason": "issue #368",
            "created_ts": "2026-10-26T01:00:00Z",
            "expires_ts": "2026-10-26T02:00:00Z",
            "released_ts": null,
        })
    }

    fn snapshot(rows: Vec<Value>, active_only: bool) -> Value {
        serde_json::json!({
            "schema_version": 1, "complete": true, "project": project(),
            "agents": [{"id": 12, "project_id": 7, "name": "BlueLake"}],
            "captured_ts": "2026-10-26T01:15:00Z", "active_only": active_only,
            "count": rows.len(), "reservations": rows,
        })
    }

    pub(crate) fn reservation_read_fixture() -> (Value, Value) {
        let mut project = project();
        project["agents"] = serde_json::json!([
            {"id":12,"project_id":7,"name":"BlueLake"}
        ]);
        let mut snapshot = snapshot(vec![reservation(1)], true);
        snapshot["project"] = project.clone();
        (project, snapshot)
    }

    #[test]
    fn automatic_reservation_reads_keep_complete_snapshots_and_registered_agents() {
        let (mut project, mut snapshot) = reservation_read_fixture();
        let idle_agent = serde_json::json!({"id":13,"project_id":7,"name":"IdleAgent"});
        project["agents"]
            .as_array_mut()
            .unwrap()
            .push(idle_agent.clone());
        snapshot["agents"].as_array_mut().unwrap().push(idle_agent);
        snapshot["reservations"] =
            serde_json::json!((1..=300).map(reservation).collect::<Vec<_>>());
        snapshot["count"] = 300.into();
        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(mailbox_payload(config)),
                    TestReply::Resource(project.clone()),
                    TestReply::Resource(snapshot),
                    TestReply::Resource(project),
                    TestReply::Resource(mailbox_payload(config)),
                ]
            },
            |config| {
                try_read_local_mailbox(
                    config,
                    &ProjectSelection::Exact("test-project".into()),
                    true,
                )
            },
        );
        let read = result.unwrap().expect("matching daemon selected");
        assert_eq!(read.reservations.len(), 300);
        assert_eq!(read.agent_names, vec!["BlueLake", "IdleAgent"]);
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests[2]["params"]["uri"],
            "resource://file_reservations/test-project?snapshot=true&active_only=true&format=json"
        );
    }

    #[test]
    fn automatic_reservations_resolve_nested_scope_only_with_the_ancestor_contract() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let root = mcp_agent_mail_core::resolve_project_path(&directory.path().to_string_lossy())
            .to_string_lossy()
            .into_owned();
        let requested = mcp_agent_mail_core::resolve_project_path(&nested.to_string_lossy())
            .to_string_lossy()
            .into_owned();
        let selection = ProjectSelection::Ancestors(requested.clone());
        let (mut project, mut snapshot) = reservation_read_fixture();
        project["human_key"] = root.clone().into();
        snapshot["project"] = project.clone();
        project["resolution"] =
            serde_json::json!({"kind":"nearest_ancestor","requested_path":requested});
        let (result, requests) = with_test_server(
            |config| {
                vec![
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                    TestReply::Resource(project.clone()),
                    TestReply::Resource(snapshot),
                    TestReply::Resource(project),
                    TestReply::Resource(ancestor_mailbox_payload(config)),
                ]
            },
            |config| try_read_local_mailbox(config, &selection, true),
        );
        assert_eq!(result.unwrap().unwrap().project.human_key, root);
        assert_eq!(
            requests[1]["params"]["uri"],
            project_resource_uri(&selection).unwrap()
        );
        assert_eq!(requests[1]["params"]["uri"], requests[3]["params"]["uri"]);
        let (result, requests) = with_test_server(
            |config| vec![TestReply::Resource(mailbox_payload(config))],
            |config| try_read_local_mailbox(config, &selection, true),
        );
        assert!(
            result.unwrap().is_none(),
            "older daemon keeps existing local scope resolution"
        );
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn automatic_reservations_refuse_changed_authority_or_incomplete_data() {
        for variant in [
            "roster",
            "project",
            "mailbox",
            "incomplete",
            "missing_project",
        ] {
            let (project, mut snapshot) = reservation_read_fixture();
            let (result, _) =
                with_test_server(
                    |config| {
                        let mut replies = vec![TestReply::Resource(mailbox_payload(config))];
                        if variant == "missing_project" {
                            replies.push(TestReply::Raw(200, serde_json::json!({
                            "jsonrpc":"2.0", "error":{"code":-32002,"message":"not registered"}
                        })));
                            return replies;
                        }
                        replies.push(TestReply::Resource(project.clone()));
                        if variant == "incomplete" {
                            snapshot["complete"] = false.into();
                        }
                        replies.push(TestReply::Resource(snapshot));
                        if variant == "incomplete" {
                            return replies;
                        }
                        let mut after = project;
                        match variant {
                            "roster" => after["agents"][0]["name"] = "RecreatedAgent".into(),
                            "project" => after["created_at"] = "2026-10-27T00:00:00Z".into(),
                            _ => (),
                        }
                        replies.push(TestReply::Resource(after));
                        if variant == "mailbox" {
                            let mut changed = mailbox_payload(config);
                            changed["storage_root"] = "/another/archive".into();
                            replies.push(TestReply::Resource(changed));
                        }
                        replies
                    },
                    |config| {
                        try_read_local_mailbox(
                            config,
                            &ProjectSelection::Exact("test-project".into()),
                            true,
                        )
                    },
                );
            assert!(
                result.is_err(),
                "{variant} must never fall back or report empty success"
            );
        }
    }

    #[test]
    fn automatic_reservations_preserve_the_legacy_completeness_bound() {
        for count in [20, LEGACY_RESOURCE_PAGE_MAX] {
            let (project, _) = reservation_read_fixture();
            let (result, requests) = with_test_server(
                |config| {
                    let mut replies = vec![
                        TestReply::Resource(mailbox_payload(config)),
                        TestReply::Resource(project.clone()),
                        TestReply::Resource(serde_json::json!([reservation(1)])),
                        TestReply::Resource(serde_json::json!(
                            (1..=i64::try_from(count).unwrap())
                                .map(reservation)
                                .collect::<Vec<_>>()
                        )),
                    ];
                    if count < LEGACY_RESOURCE_PAGE_MAX {
                        replies.extend([
                            TestReply::Resource(project),
                            TestReply::Resource(mailbox_payload(config)),
                        ]);
                    }
                    replies
                },
                |config| {
                    try_read_local_mailbox(
                        config,
                        &ProjectSelection::Exact("test-project".into()),
                        true,
                    )
                },
            );
            assert!(
                requests[3]["params"]["uri"]
                    .as_str()
                    .unwrap()
                    .contains("limit=250&offset=0")
            );
            if count < LEGACY_RESOURCE_PAGE_MAX {
                assert_eq!(result.unwrap().unwrap().reservations.len(), count);
            } else {
                assert!(
                    result.is_err(),
                    "a full legacy page cannot prove a complete listing"
                );
            }
        }
    }

    fn response(uri: &str, value: &Value) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "request-1",
            "result": { "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": value.to_string(),
            }] },
        })
    }

    #[test]
    fn resource_component_encoding_preserves_one_component() {
        assert_eq!(
            encode_resource_component("/tmp/repo?active_only=false#x%2f ü"),
            "%2Ftmp%2Frepo%3Factive_only%3Dfalse%23x%252f%20%C3%BC"
        );
        assert_eq!(encode_resource_component("repo-a_1.~"), "repo-a_1.~");
    }

    #[test]
    fn valid_resource_response_and_empty_list_are_accepted() {
        let uri = "resource://file_reservations/test?format=json";
        let value = serde_json::json!([]);
        assert_eq!(
            validate_resource_response(&response(uri, &value), "request-1", uri).unwrap(),
            value
        );
        assert!(validate_reservations(&value, true).unwrap().is_empty());
    }

    #[test]
    fn malformed_or_mismatched_resource_envelopes_fail_closed() {
        let uri = "resource://file_reservations/test?format=json";
        let base = response(uri, &serde_json::json!([]));
        let mut bad = Vec::new();
        let mut value = base.clone();
        value["jsonrpc"] = serde_json::json!("1.0");
        bad.push(value);
        let mut value = base.clone();
        value["id"] = serde_json::json!("old-request");
        bad.push(value);
        let mut value = base.clone();
        value["result"]["contents"][0]["uri"] = serde_json::json!("resource://other");
        bad.push(value);
        let mut value = base.clone();
        value["result"]["contents"][0]["text"] = serde_json::json!("not-json");
        bad.push(value);
        let mut value = base.clone();
        value["result"]["contents"] = serde_json::json!([]);
        bad.push(value);
        let mut value = base.clone();
        value["result"]["contents"]
            .as_array_mut()
            .unwrap()
            .push(base["result"]["contents"][0].clone());
        bad.push(value);
        let mut value = base.clone();
        value["result"]["contents"][0]["blob"] = serde_json::json!("ZXJyb3I=");
        bad.push(value);
        let mut value = base.clone();
        value["result"]["isError"] = serde_json::json!(true);
        bad.push(value);
        let mut value = base;
        value["error"] = serde_json::json!({ "code": -32603, "message": "secret" });
        bad.push(value);
        for value in bad {
            assert!(validate_resource_response(&value, "request-1", uri).is_err());
        }
    }

    #[test]
    fn rpc_rejection_does_not_expose_peer_error_message() {
        let value = serde_json::json!({
            "jsonrpc": "2.0", "id": "request-1",
            "error": { "code": -32001, "message": "private-token-and-body" },
        });
        let error = validate_resource_response(&value, "request-1", "resource://x")
            .unwrap_err()
            .to_string();
        assert!(error.contains("-32001"));
        assert!(!error.contains("private-token-and-body"));
    }

    #[test]
    fn transport_diagnostics_never_echo_urls_credentials_or_peer_bodies() {
        for (message, expected) in [
            (
                "authentication failed (HTTP 403) while calling https://private-token.example/mcp/",
                "authentication failed",
            ),
            (
                "unexpected HTTP status 307 from https://private-token.example/mcp/",
                "refused an HTTP redirect",
            ),
            (
                "unexpected HTTP status 503 from https://private-token.example/mcp/",
                "HTTP status 503",
            ),
            (
                "transport failure calling https://private-token.example/mcp/: reflected-private-body",
                "unavailable or timed out",
            ),
            (
                "request to https://private-token.example/mcp/ timed out after 30s",
                "unavailable or timed out",
            ),
            (
                "malformed HTTP response from https://private-token.example/mcp/: reflected-private-body",
                "HTTP/JSON response was invalid",
            ),
        ] {
            let error = sanitize_transport_error(CliError::Other(message.to_string())).to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-token"), "{error}");
            assert!(!error.contains("private-body"), "{error}");
        }
    }

    #[test]
    fn project_requires_stored_identity_not_only_a_computed_slug() {
        let value = project();
        assert!(validate_project(&value, "TEST-PROJECT").is_ok());
        assert!(validate_project(&value, "/tmp/agent-mail-reservation-reader-project").is_ok());
        assert!(validate_project(&value, "other-project").is_err());
        assert!(validate_project(&value, "/tmp/agent/mail/reservation/reader/project").is_err());
        let mut missing_epoch = value.clone();
        missing_epoch.as_object_mut().unwrap().remove("created_at");
        assert!(validate_project(&missing_epoch, "test-project").is_err());
        let mut missing_id = value;
        missing_id["id"] = serde_json::json!(0);
        assert!(validate_project(&missing_id, "test-project").is_err());
    }

    #[test]
    fn additional_project_selectors_must_agree_with_the_server_descriptor() {
        let stored = validate_project(&project(), "test-project").unwrap();
        assert!(verify_project_selector(&stored, "test-project").is_ok());
        assert!(
            verify_project_selector(&stored, " /tmp/agent-mail-reservation-reader-project ")
                .is_ok()
        );
        assert!(verify_project_selector(&stored, "different-project").is_err());
        assert!(verify_project_selector(&stored, "").is_err());
    }

    #[test]
    fn canonical_path_aliases_are_verified_without_git_identity_discovery() {
        let cwd = std::env::current_dir().unwrap();
        let mut stored = validate_project(&project(), "test-project").unwrap();
        stored.human_key = std::fs::canonicalize(&cwd)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // Only existing-directory canonicalization is needed; the reader must
        // not discover/create worktree project IDs to accept this alias.
        let alias = cwd.join(".").to_string_lossy().into_owned();
        assert!(verify_project_selector(&stored, &alias).is_ok());
        let missing = cwd
            .join("gh368-unregistered-distinct-project")
            .to_string_lossy()
            .into_owned();
        assert!(verify_project_selector(&stored, &missing).is_err());
    }

    #[test]
    fn rebound_or_recreated_projects_do_not_match() {
        let before = validate_project(&project(), "test-project").unwrap();
        assert!(same_project(&before, &before));
        for (key, replacement) in [
            ("id", serde_json::json!(8)),
            (
                "human_key",
                serde_json::json!("/tmp/different-mailbox-project"),
            ),
            ("created_at", serde_json::json!("2026-10-26T02:00:00Z")),
        ] {
            let mut changed = project();
            changed[key] = replacement;
            let after = validate_project(&changed, "test-project").unwrap();
            assert!(!same_project(&before, &after));
        }
    }

    #[test]
    fn project_roster_keeps_registered_agents_without_leases_and_ambiguities() {
        let mut value = project();
        value["agents"] = serde_json::json!([
            { "id": 12, "name": "BlueLake", "project_id": 7 },
            { "id": 11, "name": "NoLease", "project_id": 7 },
            { "id": 13, "name": "bluelake", "project_id": 7 },
        ]);
        let roster = validate_agent_roster(&value, 7).unwrap();
        assert_eq!(
            roster,
            vec![
                (11, "NoLease".into()),
                (12, "BlueLake".into()),
                (13, "bluelake".into())
            ]
        );

        let mut recreated = value.clone();
        recreated["agents"][0]["id"] = serde_json::json!(14);
        assert_ne!(roster, validate_agent_roster(&recreated, 7).unwrap());
        for bad in [
            serde_json::json!(null),
            serde_json::json!([{ "id": 1, "name": "BlueLake", "project_id": 8 }]),
            serde_json::json!([{ "id": 1, "name": "", "project_id": 7 }]),
            serde_json::json!([{ "name": "BlueLake", "project_id": 7 }]),
            serde_json::json!([
                { "id": 1, "name": "BlueLake", "project_id": 7 },
                { "id": 1, "name": "NoLease", "project_id": 7 },
            ]),
        ] {
            value["agents"] = bad;
            assert!(validate_agent_roster(&value, 7).is_err());
        }
    }

    #[test]
    fn valid_rows_preserve_exclusivity_and_timestamps() {
        let mut shared = reservation(2);
        shared["exclusive"] = serde_json::json!(false);
        shared["released_ts"] = serde_json::json!("2026-10-26T01:30:00Z");
        let rows =
            validate_reservations(&serde_json::json!([reservation(1), shared]), false).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].exclusive);
        assert_eq!(rows[0].released_ts, None);
        assert!(!rows[1].exclusive);
        assert_eq!(
            rows[1].released_ts,
            Some(rows[1].created_ts + 1_800_000_000)
        );
        assert_eq!(rows[1].expires_ts - rows[1].created_ts, 3_600_000_000);
    }

    #[test]
    fn malformed_rows_are_never_silently_dropped_or_defaulted() {
        for key in [
            "id",
            "agent",
            "path_pattern",
            "exclusive",
            "reason",
            "created_ts",
            "expires_ts",
            "released_ts",
        ] {
            let mut row = reservation(1);
            row.as_object_mut().unwrap().remove(key);
            assert!(
                validate_reservations(&serde_json::json!([row]), true).is_err(),
                "missing {key}"
            );
        }
        for (key, replacement) in [
            ("id", serde_json::json!(0)),
            ("id", serde_json::json!("1")),
            ("agent", serde_json::json!("")),
            ("path_pattern", serde_json::json!("")),
            ("exclusive", serde_json::json!(1)),
            ("reason", serde_json::json!(null)),
            ("created_ts", serde_json::json!("bad-date")),
            ("expires_ts", serde_json::json!("2026-10-25T00:00:00Z")),
            ("released_ts", serde_json::json!(42)),
            ("released_ts", serde_json::json!("2026-10-26T01:30:00Z")),
        ] {
            let mut row = reservation(1);
            row[key] = replacement;
            assert!(
                validate_reservations(&serde_json::json!([row]), true).is_err(),
                "invalid {key}"
            );
        }
        assert!(validate_reservations(&serde_json::json!({ "reservations": [] }), true).is_err());
        assert!(
            validate_reservations(&serde_json::json!([reservation(1), reservation(1)]), true,)
                .is_err()
        );
    }

    #[test]
    fn complete_snapshots_cross_the_page_limit_and_keep_retired_holders() {
        let project = validate_project(&project(), "test-project").unwrap();
        let mut value = snapshot((1..=300).map(reservation).collect(), false);
        // The active directory omits this retired identity, but the atomic
        // snapshot still resolves its historical lease by the original ID.
        value["agents"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": 13, "project_id": 7, "name": "RetiredHolder",
            }));
        value["reservations"][299]["agent_id"] = serde_json::json!(13);
        value["reservations"][299]["agent"] = serde_json::json!("RetiredHolder");
        value["reservations"][299]["released_ts"] = serde_json::json!("2026-10-26T01:10:00Z");
        let (holders, rows) = validate_snapshot(&value, &project, false).unwrap();
        assert_eq!(holders.len(), 2);
        assert_eq!(rows.len(), 300);
        assert_eq!(rows[299].agent, "RetiredHolder");
        for legacy_id in [0, -1] {
            value["agents"][1]["id"] = serde_json::json!(legacy_id);
            value["reservations"][299]["agent_id"] = serde_json::json!(legacy_id);
            assert!(validate_snapshot(&value, &project, false).is_ok());
        }
        let oversized = snapshot((1..=10_001).map(reservation).collect(), true);
        assert!(validate_snapshot(&oversized, &project, true).is_err());
    }

    #[test]
    fn legacy_complete_reads_require_a_short_explicit_page() {
        for count in [0, 20, 27, LEGACY_RESOURCE_PAGE_MAX - 1] {
            let value = serde_json::json!(
                (1..=i64::try_from(count).unwrap())
                    .map(reservation)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                validate_complete_legacy_reservations(&value, true)
                    .unwrap()
                    .len(),
                count
            );
        }
        let full = serde_json::json!((1..=250).map(reservation).collect::<Vec<_>>());
        assert!(validate_legacy_reservation_page(&full, true).is_ok());
        let error = validate_complete_legacy_reservations(&full, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("full 250-row legacy reservation page")
        );
        let oversized = serde_json::json!((1..=251).map(reservation).collect::<Vec<_>>());
        assert!(validate_legacy_reservation_page(&oversized, true).is_err());
        assert!(validate_complete_legacy_reservations(&oversized, true).is_err());
    }

    #[test]
    fn legacy_protocol_requires_valid_rows_and_never_accepts_an_envelope_as_a_page() {
        let mut row = reservation(1);
        // The released server does not return snapshot-only holder/project IDs.
        let fields = row.as_object_mut().unwrap();
        fields.remove("agent_id");
        fields.remove("project_id");
        row["agent"] = serde_json::json!("RetiredHolder");
        let rows =
            validate_complete_legacy_reservations(&serde_json::json!([row.clone()]), true).unwrap();
        assert_eq!(rows[0].agent, "RetiredHolder");
        row["exclusive"] = serde_json::json!("true");
        assert!(validate_legacy_reservation_page(&serde_json::json!([row]), true).is_err());
        for value in [
            serde_json::json!(null),
            snapshot(vec![reservation(1)], true),
            serde_json::json!({"reservations": []}),
            serde_json::json!([reservation(1), reservation(1)]),
        ] {
            assert!(validate_complete_legacy_reservations(&value, true).is_err());
        }
    }

    #[test]
    fn snapshot_authority_rejects_partial_foreign_and_stale_rows() {
        let project = validate_project(&project(), "test-project").unwrap();
        let valid = snapshot(vec![reservation(1)], true);
        assert!(validate_snapshot(&valid, &project, true).is_ok());
        assert!(validate_snapshot(&serde_json::json!([reservation(1)]), &project, true).is_err());
        for (pointer, replacement) in [
            ("/schema_version", serde_json::json!(2)),
            ("/complete", serde_json::json!(false)),
            ("/active_only", serde_json::json!(false)),
            ("/count", serde_json::json!(2)),
            ("/captured_ts", serde_json::json!("not-a-timestamp")),
            ("/captured_ts", serde_json::json!("2026-10-26T02:00:00Z")),
            (
                "/project/created_at",
                serde_json::json!("2026-10-26T02:00:00Z"),
            ),
            ("/agents/0/project_id", serde_json::json!(8)),
            ("/reservations/0/project_id", serde_json::json!(8)),
            ("/reservations/0/agent_id", serde_json::json!(99)),
            ("/reservations/0/agent", serde_json::json!("AnotherHolder")),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                validate_snapshot(&invalid, &project, true).is_err(),
                "{pointer}"
            );
        }
        let uri = "resource://file_reservations/test-project?snapshot=true";
        let mut oversized = response(uri, &valid);
        oversized["result"]["contents"][0]["text"] =
            serde_json::json!(" ".repeat(FILE_RESERVATION_SNAPSHOT_MAX_BYTES + 1));
        assert!(validate_resource_response(&oversized, "request-1", uri).is_err());
    }
}
