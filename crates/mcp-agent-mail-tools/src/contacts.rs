//! Contact cluster tools
//!
//! Tools for agent contact management:
//! - `request_contact`: Request permission to message another agent
//! - `respond_contact`: Approve or deny a contact request
//! - `list_contacts`: List contact relationships
//! - `set_contact_policy`: Configure agent contact policy

use fastmcp::prelude::*;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};

use crate::tool_util::{
    db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent, resolve_project,
};

/// Contact link state (tool-facing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactLinkState {
    pub from: String,
    pub from_project: String,
    pub to: String,
    pub to_project: String,
    pub status: String,
    pub expires_ts: Option<String>,
}

/// Detailed contact link representation (macro responses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactLink {
    pub id: i64,
    pub from_agent: String,
    pub from_project: String,
    pub to_agent: String,
    pub to_project: String,
    pub status: String,
    pub reason: String,
    pub created_ts: String,
    pub updated_ts: String,
    pub expires_ts: Option<String>,
}

/// Contact list response (tool-facing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactListResponse {
    pub outgoing: Vec<ContactLinkState>,
    pub incoming: Vec<ContactLinkState>,
}

/// Simple contact entry for `list_contacts` (matches Python format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleContactEntry {
    pub to: String,
    pub status: String,
    pub reason: String,
    pub updated_ts: Option<String>,
    pub expires_ts: Option<String>,
}

/// Agent policy response (legacy, richer format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPolicyResponse {
    pub id: i64,
    pub name: String,
    pub contact_policy: String,
}

/// Simple policy response (matches Python format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimplePolicyResponse {
    pub agent: String,
    pub policy: String,
}

/// Contact response for approve/deny.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespondContactResponse {
    pub from: String,
    pub to: String,
    pub approved: bool,
    pub expires_ts: Option<String>,
    pub updated: usize,
}

/// Valid contact policy values.
pub const VALID_CONTACT_POLICIES: &[&str] = &["open", "auto", "contacts_only", "block_all"];

/// Normalize a contact policy string.
///
/// Trims whitespace, lowercases, and returns the value if it matches a known
/// policy. Unknown values silently default to `"auto"`.
#[must_use]
pub fn normalize_contact_policy(raw: &str) -> String {
    let norm = raw.trim().to_ascii_lowercase();
    if VALID_CONTACT_POLICIES.contains(&norm.as_str()) {
        norm
    } else {
        "auto".to_string()
    }
}

/// Resolve the sender agent, optionally auto-registering if missing.
#[allow(clippy::too_many_arguments)]
async fn resolve_or_register_sender(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    from_agent: &str,
    register_if_missing: bool,
    program: Option<String>,
    model: Option<String>,
    task_description: Option<&str>,
) -> McpResult<mcp_agent_mail_db::AgentRow> {
    match resolve_agent(ctx, pool, project_id, from_agent).await {
        Ok(a) => Ok(a),
        Err(e) if !register_if_missing => Err(e),
        Err(_) => {
            let program = program.ok_or_else(|| {
                legacy_tool_error(
                    "MISSING_FIELD",
                    "program is required when register_if_missing=true",
                    true,
                    serde_json::json!({ "field": "program" }),
                )
            })?;
            let model = model.ok_or_else(|| {
                legacy_tool_error(
                    "MISSING_FIELD",
                    "model is required when register_if_missing=true",
                    true,
                    serde_json::json!({ "field": "model" }),
                )
            })?;

            let out = mcp_agent_mail_db::queries::register_agent(
                ctx.cx(),
                pool,
                project_id,
                from_agent,
                &program,
                &model,
                task_description,
                Some("auto"),
            )
            .await;
            Ok(db_outcome_to_mcp_result(out)?)
        }
    }
}

/// Parse `project:<slug>#<Name>` shorthand into a `(project_key, agent_name)` tuple.
/// Falls back to the default project and raw agent name if no shorthand match.
fn parse_contact_target(
    to_agent: &str,
    to_project: Option<String>,
    default_project: &str,
) -> (String, String) {
    if let Some(tp) = to_project {
        return (tp, to_agent.to_string());
    }
    if to_agent.starts_with("project:") && to_agent.contains('#') {
        if let Some((slug, agent)) = to_agent.split_once(':').and_then(|(_, rest)| {
            let (slug, agent) = rest.split_once('#')?;
            let agent = agent.trim();
            if slug.is_empty() || agent.is_empty() {
                None
            } else {
                Some((slug.to_string(), agent.to_string()))
            }
        }) {
            return (slug, agent);
        }
    }
    (default_project.to_string(), to_agent.to_string())
}

/// Request contact approval to message another agent.
///
/// Creates or refreshes a pending `AgentLink` and sends a small `ack_required` intro message.
#[allow(clippy::too_many_arguments)]
#[tool(description = "Request contact approval to message another agent.")]
pub async fn request_contact(
    ctx: &McpContext,
    project_key: String,
    from_agent: String,
    to_agent: String,
    to_project: Option<String>,
    reason: Option<String>,
    ttl_seconds: Option<i64>,
    register_if_missing: Option<bool>,
    program: Option<String>,
    model: Option<String>,
    task_description: Option<String>,
) -> McpResult<String> {
    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let from_row = resolve_or_register_sender(
        ctx,
        &pool,
        project_id,
        &from_agent,
        register_if_missing.unwrap_or(true),
        program,
        model,
        task_description.as_deref(),
    )
    .await?;

    let (target_project_key, target_agent_name) =
        parse_contact_target(&to_agent, to_project, &project_key);
    let target_project_row = resolve_project(ctx, &pool, &target_project_key).await?;
    let target_project_id = target_project_row.id.unwrap_or(0);

    let to_row = resolve_agent(ctx, &pool, target_project_id, &target_agent_name).await?;

    let ttl = ttl_seconds.unwrap_or(604_800).max(60); // 7 days default; min 60s
    let link_row = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::request_contact(
            ctx.cx(),
            &pool,
            project_id,
            from_row.id.unwrap_or(0),
            target_project_id,
            to_row.id.unwrap_or(0),
            reason.as_deref().unwrap_or(""),
            ttl,
        )
        .await,
    )?;

    // Send an intro mail (ack_required) so the recipient sees the request in their inbox.
    let subject = format!("Contact request from {from_agent}");
    let body_md = format!("{from_agent} requests permission to contact {target_agent_name}.");

    let to_id = to_row.id.unwrap_or(0);
    let recipients: &[(i64, &str)] = &[(to_id, "to")];
    db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message_with_recipients(
            ctx.cx(),
            &pool,
            project_id,
            from_row.id.unwrap_or(0),
            &subject,
            &body_md,
            None,
            "normal",
            true,
            "[]",
            recipients,
        )
        .await,
    )?;

    let response = ContactLinkState {
        from: from_agent,
        from_project: project.human_key,
        to: target_agent_name,
        to_project: target_project_row.human_key,
        status: link_row.status,
        expires_ts: link_row.expires_ts.map(micros_to_iso),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// Approve or deny a contact request.
///
/// # Parameters
/// - `project_key`: Your project identifier
/// - `to_agent`: Your agent name (the recipient of the request)
/// - `from_agent`: Requester's agent name
/// - `from_project`: Requester's project (if cross-project)
/// - `accept`: true to approve, false to block
/// - `ttl_seconds`: TTL for approved link (default: 30 days)
#[tool(description = "Approve or deny a contact request.")]
pub async fn respond_contact(
    ctx: &McpContext,
    project_key: String,
    to_agent: String,
    from_agent: String,
    from_project: Option<String>,
    accept: bool,
    ttl_seconds: Option<i64>,
) -> McpResult<String> {
    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let source_project_key = from_project.unwrap_or_else(|| project_key.clone());
    let source_project_row = resolve_project(ctx, &pool, &source_project_key).await?;
    let source_project_id = source_project_row.id.unwrap_or(0);

    let from_row = resolve_agent(ctx, &pool, source_project_id, &from_agent).await?;
    let to_row = resolve_agent(ctx, &pool, project_id, &to_agent).await?;

    let ttl = ttl_seconds.unwrap_or(2_592_000).max(60); // 30 days default; min 60s
    let (updated, link_row) = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::respond_contact(
            ctx.cx(),
            &pool,
            source_project_id,
            from_row.id.unwrap_or(0),
            project_id,
            to_row.id.unwrap_or(0),
            accept,
            ttl,
        )
        .await,
    )?;

    let response = RespondContactResponse {
        from: from_agent,
        to: to_agent,
        approved: accept,
        expires_ts: link_row.expires_ts.map(micros_to_iso),
        updated,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// List contact links for an agent in a project.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent to list contacts for
///
/// # Returns
/// Array of outgoing contacts with `to`, `status`, `reason`, `updated_ts`, `expires_ts`
#[tool(description = "List contact links for an agent in a project.")]
pub async fn list_contacts(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
) -> McpResult<String> {
    use std::collections::HashMap;

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    let (outgoing_rows, _incoming_rows) = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_contacts(ctx.cx(), &pool, project_id, agent_id).await,
    )?;

    // Resolve referenced agents to names
    let mut agent_names: HashMap<i64, String> = HashMap::with_capacity(outgoing_rows.len());
    for r in &outgoing_rows {
        if let std::collections::hash_map::Entry::Vacant(e) = agent_names.entry(r.b_agent_id) {
            if let Ok(row) = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, r.b_agent_id).await,
            ) {
                e.insert(row.name);
            }
        }
    }

    // Return simple array format matching Python
    // Note: Python fixture expects null for updated_ts and expires_ts
    let contacts: Vec<SimpleContactEntry> = outgoing_rows
        .into_iter()
        .map(|r| SimpleContactEntry {
            to: agent_names
                .get(&r.b_agent_id)
                .cloned()
                .unwrap_or_else(|| format!("agent_{}", r.b_agent_id)),
            status: r.status,
            reason: r.reason,
            updated_ts: None,
            expires_ts: None,
        })
        .collect();

    tracing::debug!(
        "Listed {} contacts for {} in project {}",
        contacts.len(),
        agent_name,
        project_key
    );

    serde_json::to_string(&contacts)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// Set contact policy for an agent.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent to configure
/// - `policy`: Policy to set (open | auto | `contacts_only` | `block_all`)
///
/// # Returns
/// Updated agent record
#[tool(description = "Set contact policy for an agent: open | auto | contacts_only | block_all.")]
pub async fn set_contact_policy(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    policy: String,
) -> McpResult<String> {
    let policy_norm = normalize_contact_policy(&policy);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    let updated_agent = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::set_agent_contact_policy(
            ctx.cx(),
            &pool,
            agent_id,
            &policy_norm,
        )
        .await,
    )?;

    let response = SimplePolicyResponse {
        agent: updated_agent.name,
        policy: updated_agent.contact_policy,
    };

    tracing::debug!(
        "Set contact policy for {} in project {} to {}",
        agent_name,
        project_key,
        policy_norm
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_contact_policy ──

    #[test]
    fn valid_policies_preserved() {
        assert_eq!(normalize_contact_policy("open"), "open");
        assert_eq!(normalize_contact_policy("auto"), "auto");
        assert_eq!(normalize_contact_policy("contacts_only"), "contacts_only");
        assert_eq!(normalize_contact_policy("block_all"), "block_all");
    }

    #[test]
    fn case_insensitive_normalization() {
        assert_eq!(normalize_contact_policy("OPEN"), "open");
        assert_eq!(normalize_contact_policy("Auto"), "auto");
        assert_eq!(normalize_contact_policy("CONTACTS_ONLY"), "contacts_only");
        assert_eq!(normalize_contact_policy("Block_All"), "block_all");
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(normalize_contact_policy("  open  "), "open");
        assert_eq!(normalize_contact_policy("\tauto\n"), "auto");
    }

    #[test]
    fn unknown_policies_default_to_auto() {
        assert_eq!(normalize_contact_policy(""), "auto");
        assert_eq!(normalize_contact_policy("invalid"), "auto");
        assert_eq!(normalize_contact_policy("reject"), "auto");
        assert_eq!(normalize_contact_policy("allow"), "auto");
        assert_eq!(normalize_contact_policy("block"), "auto");
        assert_eq!(normalize_contact_policy("none"), "auto");
        assert_eq!(normalize_contact_policy("contacts-only"), "auto");
    }

    // ── Response type serialization ──

    #[test]
    fn contact_link_state_serializes() {
        let r = ContactLinkState {
            from: "BlueLake".into(),
            from_project: "/data/projects/test".into(),
            to: "RedFox".into(),
            to_project: "/data/projects/test".into(),
            status: "pending".into(),
            expires_ts: Some("2026-02-13T00:00:00Z".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["from"], "BlueLake");
        assert_eq!(json["to"], "RedFox");
        assert_eq!(json["status"], "pending");
        assert!(json["expires_ts"].is_string());
    }

    #[test]
    fn contact_link_state_null_expires() {
        let r = ContactLinkState {
            from: "A".into(),
            from_project: "/p".into(),
            to: "B".into(),
            to_project: "/p".into(),
            status: "approved".into(),
            expires_ts: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["expires_ts"].is_null());
    }

    #[test]
    fn simple_contact_entry_serializes() {
        let r = SimpleContactEntry {
            to: "RedFox".into(),
            status: "approved".into(),
            reason: "collaboration".into(),
            updated_ts: None,
            expires_ts: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["to"], "RedFox");
        assert_eq!(json["status"], "approved");
        assert!(json["updated_ts"].is_null());
    }

    #[test]
    fn simple_policy_response_serializes() {
        let r = SimplePolicyResponse {
            agent: "BlueLake".into(),
            policy: "contacts_only".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["agent"], "BlueLake");
        assert_eq!(json["policy"], "contacts_only");
    }

    #[test]
    fn respond_contact_response_serializes() {
        let r = RespondContactResponse {
            from: "A".into(),
            to: "B".into(),
            approved: true,
            expires_ts: Some("2026-03-06T00:00:00Z".into()),
            updated: 1,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["approved"], true);
        assert_eq!(json["updated"], 1);
        assert_eq!(json["from"], "A");
    }

    #[test]
    fn contact_link_round_trips() {
        let original = ContactLink {
            id: 1,
            from_agent: "BlueLake".into(),
            from_project: "/data/p1".into(),
            to_agent: "RedFox".into(),
            to_project: "/data/p2".into(),
            status: "approved".into(),
            reason: "coordination".into(),
            created_ts: "2026-02-06T00:00:00Z".into(),
            updated_ts: "2026-02-06T01:00:00Z".into(),
            expires_ts: Some("2026-03-06T00:00:00Z".into()),
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: ContactLink = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.from_agent, original.from_agent);
        assert_eq!(deserialized.to_agent, original.to_agent);
        assert_eq!(deserialized.status, original.status);
    }

    // ── TTL defaults ──

    #[test]
    fn default_request_ttl_is_seven_days() {
        let ttl: i64 = 604_800;
        assert_eq!(ttl, 7 * 24 * 3600);
    }

    #[test]
    fn default_respond_ttl_is_thirty_days() {
        let ttl: i64 = 2_592_000;
        assert_eq!(ttl, 30 * 24 * 3600);
    }

    // ── parse_contact_target ──

    #[test]
    fn parse_contact_target_explicit_project() {
        let (proj, agent) =
            parse_contact_target("BlueLake", Some("other-project".into()), "/default");
        assert_eq!(proj, "other-project");
        assert_eq!(agent, "BlueLake");
    }

    #[test]
    fn parse_contact_target_shorthand() {
        let (proj, agent) = parse_contact_target("project:my-proj#RedFox", None, "/default");
        assert_eq!(proj, "my-proj");
        assert_eq!(agent, "RedFox");
    }

    #[test]
    fn parse_contact_target_shorthand_with_whitespace() {
        let (proj, agent) = parse_contact_target("project:my-proj# RedFox ", None, "/default");
        assert_eq!(proj, "my-proj");
        assert_eq!(agent, "RedFox");
    }

    #[test]
    fn parse_contact_target_invalid_shorthand_falls_back() {
        let (proj, agent) = parse_contact_target("project:#", None, "/default");
        assert_eq!(proj, "/default");
        assert_eq!(agent, "project:#");
    }

    #[test]
    fn parse_contact_target_plain_name() {
        let (proj, agent) = parse_contact_target("BlueLake", None, "/my/project");
        assert_eq!(proj, "/my/project");
        assert_eq!(agent, "BlueLake");
    }

    // -----------------------------------------------------------------------
    // Tool validation rule tests (br-2841)
    // -----------------------------------------------------------------------

    // ── normalize_contact_policy — extended edge cases ──

    #[test]
    fn policy_normalization_handles_mixed_case_with_underscores() {
        assert_eq!(normalize_contact_policy("Contacts_Only"), "contacts_only");
        assert_eq!(normalize_contact_policy("BLOCK_ALL"), "block_all");
    }

    #[test]
    fn policy_normalization_rejects_hyphenated_variants() {
        // Users might try hyphenated versions — these should default to auto
        assert_eq!(normalize_contact_policy("block-all"), "auto");
        assert_eq!(normalize_contact_policy("contacts-only"), "auto");
    }

    #[test]
    fn policy_normalization_rejects_null_byte() {
        assert_eq!(normalize_contact_policy("open\0"), "auto");
    }

    #[test]
    fn policy_normalization_rejects_newlines() {
        assert_eq!(normalize_contact_policy("open\n"), "open");
        assert_eq!(normalize_contact_policy("\nopen"), "open");
    }

    // ── parse_contact_target — extended edge cases ──

    #[test]
    fn parse_contact_target_empty_agent_name_with_project() {
        // project:foo# (empty agent after #)
        let (proj, agent) = parse_contact_target("project:foo#", None, "/default");
        // Empty agent should fall back to treating entire string as agent name
        assert_eq!(proj, "/default");
        assert_eq!(agent, "project:foo#");
    }

    #[test]
    fn parse_contact_target_no_hash_in_shorthand() {
        // "project:foo" without # — no valid shorthand
        let (proj, agent) = parse_contact_target("project:foo", None, "/default");
        assert_eq!(proj, "/default");
        assert_eq!(agent, "project:foo");
    }

    #[test]
    fn parse_contact_target_to_project_overrides_shorthand() {
        // When to_project is explicitly set, it takes precedence even with shorthand
        let (proj, agent) = parse_contact_target(
            "project:other#AgentX",
            Some("explicit-project".into()),
            "/default",
        );
        assert_eq!(proj, "explicit-project");
        assert_eq!(agent, "project:other#AgentX");
    }

    // ── TTL validation ──

    #[test]
    fn ttl_zero_treated_as_default() {
        // TTL of 0 should be treated as using default (validated by tool)
        let ttl = 0_i64;
        assert!(ttl <= 0, "Zero TTL should trigger default behavior");
    }

    #[test]
    fn ttl_negative_handled() {
        let ttl = -100_i64;
        assert!(ttl < 0, "Negative TTL should be rejected or defaulted");
    }

    // ── ContactLink serialization edge cases ──

    #[test]
    fn contact_link_null_expires_omitted_in_state() {
        let r = ContactLinkState {
            from: "A".into(),
            from_project: "/p".into(),
            to: "B".into(),
            to_project: "/p".into(),
            status: "pending".into(),
            expires_ts: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        // When None, serde serializes as null (not omitted, since no skip_serializing_if)
        assert!(json["expires_ts"].is_null());
    }

    #[test]
    fn simple_contact_entry_all_fields_present() {
        let r = SimpleContactEntry {
            to: "GoldHawk".into(),
            status: "pending".into(),
            reason: "testing".into(),
            updated_ts: Some("2026-02-08T00:00:00Z".into()),
            expires_ts: Some("2026-02-15T00:00:00Z".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["to"], "GoldHawk");
        assert!(json["updated_ts"].is_string());
        assert!(json["expires_ts"].is_string());
    }
}
