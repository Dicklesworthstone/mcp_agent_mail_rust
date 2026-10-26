//! Explicit, authenticated reservation reads through the running MCP server.
//!
//! A fresh CLI database connection can be refused while the server's admitted
//! pool remains healthy (GH#368). The caller explicitly selects this authority;
//! this module never opens a database, repairs a mailbox, or guesses from an
//! archive. Project identity and the complete response are checked before any
//! reservation output is made available to the caller.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use mcp_agent_mail_core::Config;
use serde_json::Value;

use crate::context::ResolvedProject;
use crate::{CliError, CliResult};

// The existing MCP resource caps pages at 250. An unpaged CLI listing must
// never mistake a capped page for the complete set of live reservations.
const RESOURCE_PAGE_MAX: usize = 250;
const RESOURCE_TIMEOUT_SECONDS: u64 = 30;

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

    // Pin the endpoint and credential for all three reads. In particular, never
    // retry an authentication rejection on a different endpoint or in SQLite.
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
    // HTTP's existing blocking path rejects redirects. Pin the same policy
    // for HTTPS: a redirect must not change the selected mailbox authority or
    // forward its credential to a different resource on the same origin.
    let client = asupersync::http::h1::HttpClient::builder()
        .no_redirects()
        .no_retries()
        .build();
    crate::context::run_async(async {
        let project_uri = format!(
            "resource://project/{}?format=json",
            encode_resource_component(project_key)
        );
        let before =
            read_resource_json(&client, &server_url, bearer.as_deref(), &project_uri).await?;
        let project = validate_project(&before, project_key)?;
        let agent_roster = validate_agent_roster(&before, project.id)?;

        let reservation_uri = format!(
            "resource://file_reservations/{}?active_only={active_only}&limit={RESOURCE_PAGE_MAX}&offset=0&format=json",
            encode_resource_component(&project.slug)
        );
        let payload =
            read_resource_json(&client, &server_url, bearer.as_deref(), &reservation_uri).await?;
        let reservations = validate_reservations(&payload, active_only)?;

        // The identity resource computes a filesystem identity even for an
        // unregistered path. Re-read the actual stored project descriptor,
        // including its creation epoch, so a deleted/recreated or rebound
        // project cannot lend authority to a stale reservation response.
        let after =
            read_resource_json(&client, &server_url, bearer.as_deref(), &project_uri).await?;
        let after_project = validate_project(&after, project_key)?;
        let after_agent_roster = validate_agent_roster(&after, after_project.id)?;
        if !same_project(&project, &after_project) || agent_roster != after_agent_roster {
            return Err(CliError::Other(
                "server project identity or agent roster changed during reservation read; refusing unverified reservation data"
                    .to_string(),
            ));
        }

        Ok(ServerReservationRead {
            project,
            agent_names: agent_roster.into_iter().map(|(_, name)| name).collect(),
            reservations,
        })
    })
}

fn encode_resource_component(value: &str) -> String {
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
) -> CliResult<Value> {
    let id = format!("cli-reservations-read-{uri}");
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
        RESOURCE_TIMEOUT_SECONDS,
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
            "server reservation read authentication failed; check AGENT_MAIL_TOKEN/HTTP_BEARER_TOKEN"
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
                "server reservation read refused an HTTP redirect ({status}); configure the final trusted AGENT_MAIL_URL explicitly"
            )
        } else {
            format!("server reservation read failed with HTTP status {status}")
        });
    }
    if matches!(error, CliError::Io(_))
        || message.starts_with("transport failure calling ")
        || message.starts_with("request to ")
    {
        return CliError::Other(
            "server reservation resource unavailable or timed out; check AGENT_MAIL_URL/HTTP_HOST/HTTP_PORT/HTTP_PATH"
                .to_string(),
        );
    }
    CliError::Other(
        "server reservation resource read failed because the endpoint configuration or HTTP/JSON response was invalid; no reservation data accepted"
            .to_string(),
    )
}

fn invalid_response(detail: &str) -> CliError {
    CliError::Other(format!(
        "invalid server reservation response: {detail}; refusing unverified reservation data"
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
                "server rejected reservation resource read (JSON-RPC error {code}); verify the project and server authentication"
            ),
            None => "server rejected reservation resource read with a malformed JSON-RPC error"
                .to_string(),
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

fn validate_project(value: &Value, requested_key: &str) -> CliResult<ResolvedProject> {
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
            "server project identity does not match the requested project; refusing unverified reservation data"
                .to_string(),
        ));
    }
    Ok(())
}

fn same_project(left: &ResolvedProject, right: &ResolvedProject) -> bool {
    left.id == right.id
        && left.slug == right.slug
        && left.human_key == right.human_key
        && left.created_at == right.created_at
}

fn validate_agent_roster(value: &Value, project_id: i64) -> CliResult<Vec<(i64, String)>> {
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
        let id = positive_id(agent)?;
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
    if rows.len() > RESOURCE_PAGE_MAX {
        return Err(invalid_response(
            "reservation response exceeds the requested page",
        ));
    }
    if rows.len() == RESOURCE_PAGE_MAX {
        return Err(CliError::Other(format!(
            "server reservation resource returned a full {RESOURCE_PAGE_MAX}-row page; a complete reservation list cannot be verified. Use a direct CLI read when admission is available, or inspect manual MCP resource pages without treating them as a complete snapshot"
        )));
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

#[cfg(test)]
mod tests {
    use super::*;

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
            "agent": "BlueLake",
            "path_pattern": "src/**",
            "exclusive": true,
            "reason": "issue #368",
            "created_ts": "2026-10-26T01:00:00Z",
            "expires_ts": "2026-10-26T02:00:00Z",
            "released_ts": null,
        })
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
    fn complete_lists_reject_saturated_and_oversized_pages() {
        let values = (1..=250).map(reservation).collect::<Vec<_>>();
        let value = serde_json::json!(values);
        let error = validate_reservations(&value, true).unwrap_err().to_string();
        assert!(error.contains("complete reservation list cannot be verified"));
        let mut values = value.as_array().unwrap().clone();
        values.push(reservation(251));
        assert!(validate_reservations(&serde_json::json!(values), true).is_err());
        let short = (1..250).map(reservation).collect::<Vec<_>>();
        assert_eq!(
            validate_reservations(&serde_json::json!(short), true)
                .unwrap()
                .len(),
            249
        );
    }
}
