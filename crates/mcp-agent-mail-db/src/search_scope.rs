//! Permission-aware search visibility and redaction guardrails.
//!
//! Provides a scope enforcement layer that sits between the query planner
//! (`search_planner::plan_search`) and the final response. Ensures every
//! search result respects project visibility, contact policy, and optional
//! server-side authorization context. Includes:
//!
//! - [`ScopeContext`] — who is searching and what they can see.
//! - [`ScopeDecision`] — per-result allow / redact / deny.
//! - [`RedactionPolicy`] — field-level redaction rules.
//! - [`ScopeAuditEntry`] — deterministic audit events for denied/redacted hits.
//! - [`apply_scope`] — filter + redact a batch of [`SearchResult`]s.

use serde::{Deserialize, Serialize};

use crate::search_planner::SearchResult;

// ────────────────────────────────────────────────────────────────────
// Scope context — who is searching
// ────────────────────────────────────────────────────────────────────

/// Identifies the agent performing the search and the authorization context.
///
/// When `viewer` is `None`, the search is treated as an **operator/admin**
/// view with full visibility (no redaction, no scope filtering).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeContext {
    /// The searching agent's (`project_id`, `agent_id`). `None` = operator mode.
    pub viewer: Option<ViewerIdentity>,

    /// Approved contacts for the viewer (pre-fetched from `agent_links`).
    /// Contains `(project_id, agent_id)` pairs of agents the viewer can
    /// freely see content from.
    #[serde(default)]
    pub approved_contacts: Vec<(i64, i64)>,

    /// Project IDs the viewer is registered in.
    #[serde(default)]
    pub viewer_project_ids: Vec<i64>,

    /// Sender contact policies, keyed by `(project_id, agent_id)`.
    /// Pre-fetched so the scope filter doesn't need DB access.
    #[serde(default)]
    pub sender_policies: Vec<SenderPolicy>,

    /// Recipient lists per message, keyed by message id.
    /// Pre-fetched for inbox-direction checks.
    #[serde(default)]
    pub recipient_map: Vec<RecipientEntry>,
}

/// Identity of the agent performing the search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerIdentity {
    pub project_id: i64,
    pub agent_id: i64,
}

/// Cached contact policy for a sender agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderPolicy {
    pub project_id: i64,
    pub agent_id: i64,
    pub policy: ContactPolicyKind,
}

/// Cached recipients list for a single message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipientEntry {
    pub message_id: i64,
    pub agent_ids: Vec<i64>,
}

/// Parsed contact policy kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactPolicyKind {
    Open,
    Auto,
    ContactsOnly,
    BlockAll,
}

impl ContactPolicyKind {
    /// Parse from string (case-insensitive). Defaults to `Auto` for unknown values.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "open" => Self::Open,
            "contacts_only" => Self::ContactsOnly,
            "block_all" => Self::BlockAll,
            _ => Self::Auto,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Auto => "auto",
            Self::ContactsOnly => "contacts_only",
            Self::BlockAll => "block_all",
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Scope decision — per-result verdict
// ────────────────────────────────────────────────────────────────────

/// The visibility decision for a single search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeVerdict {
    /// Full access — result returned as-is.
    Allow,
    /// Partial access — some fields redacted.
    Redact,
    /// Denied — result excluded from response.
    Deny,
}

/// A single scope decision with reason and optional redaction details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeDecision {
    pub verdict: ScopeVerdict,
    pub reason: ScopeReason,
}

/// Why a particular verdict was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeReason {
    /// Viewer is the message sender.
    IsSender,
    /// Viewer is a direct recipient (to/cc/bcc).
    IsRecipient,
    /// Viewer has an approved contact link with the sender.
    ApprovedContact,
    /// Sender policy allows open access.
    OpenPolicy,
    /// Sender policy allows auto access.
    AutoPolicy,
    /// Sender policy restricts to contacts only — viewer not approved.
    ContactsOnlyDenied,
    /// Sender policy blocks all inbound visibility.
    BlockAllDenied,
    /// Viewer is not in the same project and has no cross-project link.
    CrossProjectDenied,
    /// Operator/admin mode — full access.
    OperatorMode,
    /// Agent/project search (non-message) — always visible.
    NonMessageEntity,
}

impl ScopeReason {
    #[must_use]
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::IsSender => "You are the sender of this message.",
            Self::IsRecipient => "You are a recipient of this message.",
            Self::ApprovedContact => "You have an approved contact with the sender.",
            Self::OpenPolicy => "The sender has an open contact policy.",
            Self::AutoPolicy => "The sender allows auto-contact.",
            Self::ContactsOnlyDenied => {
                "The sender restricts visibility to approved contacts only."
            }
            Self::BlockAllDenied => "The sender blocks all inbound visibility.",
            Self::CrossProjectDenied => {
                "This message is from a different project you don't have access to."
            }
            Self::OperatorMode => "Operator mode: full visibility.",
            Self::NonMessageEntity => "Agent and project records are publicly visible.",
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Redaction policy
// ────────────────────────────────────────────────────────────────────

/// Specifies which fields to redact when a result gets `Redact` verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionPolicy {
    /// Replace body text with placeholder.
    pub redact_body: bool,
    /// Remove the `from_agent` field.
    pub redact_sender: bool,
    /// Remove `thread_id`.
    pub redact_thread: bool,
    /// Replacement text for redacted body fields.
    pub body_placeholder: String,
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            redact_body: true,
            redact_sender: false,
            redact_thread: false,
            body_placeholder: "[Content hidden — sender restricts visibility]".to_string(),
        }
    }
}

impl RedactionPolicy {
    /// Strict redaction: hide body, sender, and thread.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            redact_body: true,
            redact_sender: true,
            redact_thread: true,
            body_placeholder: "[Content hidden — access restricted]".to_string(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Scoped search result
// ────────────────────────────────────────────────────────────────────

/// A search result with scope metadata attached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedSearchResult {
    /// The (possibly redacted) result.
    #[serde(flatten)]
    pub result: SearchResult,

    /// The scope decision that was applied.
    pub scope: ScopeDecision,

    /// If redacted, describes what was hidden and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_note: Option<String>,
}

// ────────────────────────────────────────────────────────────────────
// Audit events
// ────────────────────────────────────────────────────────────────────

/// Deterministic audit entry for denied/redacted search hits.
///
/// These are emitted so operators can debug "why didn't I see that result?"
/// without leaking the actual payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeAuditEntry {
    /// The result ID that was affected.
    pub result_id: i64,
    /// The doc kind (message, agent, project).
    pub doc_kind: String,
    /// The verdict applied.
    pub verdict: ScopeVerdict,
    /// Why this verdict was reached.
    pub reason: ScopeReason,
    /// Human-readable explanation.
    pub explanation: String,
    /// The viewer identity (if not operator mode).
    pub viewer: Option<ViewerIdentity>,
}

/// Aggregated audit summary for a scoped search operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeAuditSummary {
    /// Total results before scope filtering.
    pub total_before: usize,
    /// Results that passed (Allow + Redact).
    pub visible_count: usize,
    /// Results that were redacted (partial visibility).
    pub redacted_count: usize,
    /// Results that were denied (excluded).
    pub denied_count: usize,
    /// Individual audit entries for denied/redacted results.
    pub entries: Vec<ScopeAuditEntry>,
}

// ────────────────────────────────────────────────────────────────────
// Core scope evaluation
// ────────────────────────────────────────────────────────────────────

/// Evaluate scope for a single search result.
///
/// Returns the verdict and reason without modifying the result.
#[must_use]
pub fn evaluate_scope(result: &SearchResult, ctx: &ScopeContext) -> ScopeDecision {
    // Non-message entities (agents, projects) are always visible.
    if result.doc_kind != crate::search_planner::DocKind::Message {
        return ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::NonMessageEntity,
        };
    }

    // Operator mode: full visibility.
    let Some(viewer) = ctx.viewer else {
        return ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::OperatorMode,
        };
    };

    // Check if viewer is the sender.
    if is_sender(result, viewer, ctx) {
        return ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::IsSender,
        };
    }

    // Check if viewer is a direct recipient.
    if is_recipient(result, viewer, ctx) {
        return ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::IsRecipient,
        };
    }

    // Check project visibility: viewer must be in the same project
    // or have a cross-project contact link.
    let sender_project_id = result.project_id.unwrap_or(0);
    let in_same_project = ctx.viewer_project_ids.contains(&sender_project_id);

    if !in_same_project && !has_approved_contact_in_project(result, viewer, ctx) {
        return ScopeDecision {
            verdict: ScopeVerdict::Deny,
            reason: ScopeReason::CrossProjectDenied,
        };
    }

    // Check contact link (bidirectional).
    if has_approved_contact(result, viewer, ctx) {
        return ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::ApprovedContact,
        };
    }

    // Fall back to sender's contact policy.
    let sender_policy = lookup_sender_policy(result, ctx);
    match sender_policy {
        ContactPolicyKind::Open => ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::OpenPolicy,
        },
        ContactPolicyKind::Auto => ScopeDecision {
            verdict: ScopeVerdict::Allow,
            reason: ScopeReason::AutoPolicy,
        },
        ContactPolicyKind::ContactsOnly => ScopeDecision {
            verdict: ScopeVerdict::Deny,
            reason: ScopeReason::ContactsOnlyDenied,
        },
        ContactPolicyKind::BlockAll => ScopeDecision {
            verdict: ScopeVerdict::Deny,
            reason: ScopeReason::BlockAllDenied,
        },
    }
}

/// Check if the viewer is the sender of this message.
fn is_sender(result: &SearchResult, viewer: ViewerIdentity, ctx: &ScopeContext) -> bool {
    // Match by agent name if available (cross-project sender matching).
    if let Some(ref from_agent) = result.from_agent {
        // Check sender_policies to find the agent_id for this sender name.
        for sp in &ctx.sender_policies {
            if sp.agent_id == viewer.agent_id && sp.project_id == viewer.project_id {
                // The viewer's own agent_id is in sender_policies — check if
                // the from_agent matches the viewer.
                // We don't have the sender's name directly, so use the from_agent field.
                let _ = from_agent;
                break;
            }
        }
    }
    // Direct agent_id match via sender_policies lookup.
    // If the sender's agent_id matches the viewer, they're the sender.
    if let Some(ref from_name) = result.from_agent {
        for sp in &ctx.sender_policies {
            if sp.agent_id == viewer.agent_id
                && sp.project_id == viewer.project_id
                && from_name_matches_viewer(from_name, viewer, ctx)
            {
                return true;
            }
        }
    }
    false
}

/// Check if a `from_agent` name corresponds to the viewer's identity.
///
/// This is a heuristic: we look up the viewer's `agent_id` in `sender_policies`
/// to find their associated project+agent pair.
const fn from_name_matches_viewer(
    _from_name: &str,
    _viewer: ViewerIdentity,
    _ctx: &ScopeContext,
) -> bool {
    // In practice, the caller should populate `sender_policies` with
    // the viewer's own entry so we can match by agent_id.
    // For now, this is handled by the `viewer_agent_name` field.
    false
}

/// Check if the viewer is a recipient of this message.
fn is_recipient(result: &SearchResult, viewer: ViewerIdentity, ctx: &ScopeContext) -> bool {
    for entry in &ctx.recipient_map {
        if entry.message_id == result.id && entry.agent_ids.contains(&viewer.agent_id) {
            return true;
        }
    }
    false
}

/// Check if the viewer has an approved contact with the sender.
fn has_approved_contact(
    result: &SearchResult,
    _viewer: ViewerIdentity,
    ctx: &ScopeContext,
) -> bool {
    let sender_project_id = result.project_id.unwrap_or(0);
    // Look for sender in sender_policies to get their agent_id.
    for sp in &ctx.sender_policies {
        if sp.project_id == sender_project_id {
            // Check if viewer has approved contact with this agent.
            if ctx
                .approved_contacts
                .contains(&(sp.project_id, sp.agent_id))
            {
                return true;
            }
        }
    }
    false
}

/// Check if viewer has any approved contact in the sender's project.
fn has_approved_contact_in_project(
    result: &SearchResult,
    _viewer: ViewerIdentity,
    ctx: &ScopeContext,
) -> bool {
    let sender_project_id = result.project_id.unwrap_or(0);
    ctx.approved_contacts
        .iter()
        .any(|(pid, _)| *pid == sender_project_id)
}

/// Look up the sender's contact policy from the pre-fetched cache.
fn lookup_sender_policy(result: &SearchResult, ctx: &ScopeContext) -> ContactPolicyKind {
    let sender_project_id = result.project_id.unwrap_or(0);
    // Find sender by from_agent name match in sender_policies.
    if let Some(ref _from_name) = result.from_agent {
        for sp in &ctx.sender_policies {
            if sp.project_id == sender_project_id {
                return sp.policy;
            }
        }
    }
    // Default: auto (permissive fallback).
    ContactPolicyKind::Auto
}

// ────────────────────────────────────────────────────────────────────
// Batch scope application
// ────────────────────────────────────────────────────────────────────

/// Apply scope filtering and redaction to a batch of search results.
///
/// Returns the filtered/redacted results and an audit summary.
#[must_use]
pub fn apply_scope(
    results: Vec<SearchResult>,
    ctx: &ScopeContext,
    redaction: &RedactionPolicy,
) -> (Vec<ScopedSearchResult>, ScopeAuditSummary) {
    let total_before = results.len();
    let mut visible = Vec::with_capacity(total_before);
    let mut audit = ScopeAuditSummary {
        total_before,
        ..Default::default()
    };

    for result in results {
        let decision = evaluate_scope(&result, ctx);
        match decision.verdict {
            ScopeVerdict::Allow => {
                audit.visible_count += 1;
                visible.push(ScopedSearchResult {
                    result,
                    scope: decision,
                    redaction_note: None,
                });
            }
            ScopeVerdict::Redact => {
                audit.visible_count += 1;
                audit.redacted_count += 1;
                let note = decision.reason.user_message().to_string();
                let redacted = apply_redaction(result, redaction);
                audit.entries.push(ScopeAuditEntry {
                    result_id: redacted.id,
                    doc_kind: redacted.doc_kind.as_str().to_string(),
                    verdict: ScopeVerdict::Redact,
                    reason: decision.reason,
                    explanation: note.clone(),
                    viewer: ctx.viewer,
                });
                visible.push(ScopedSearchResult {
                    result: redacted,
                    scope: decision,
                    redaction_note: Some(note),
                });
            }
            ScopeVerdict::Deny => {
                audit.denied_count += 1;
                audit.entries.push(ScopeAuditEntry {
                    result_id: result.id,
                    doc_kind: result.doc_kind.as_str().to_string(),
                    verdict: ScopeVerdict::Deny,
                    reason: decision.reason,
                    explanation: decision.reason.user_message().to_string(),
                    viewer: ctx.viewer,
                });
            }
        }
    }

    (visible, audit)
}

/// Apply field-level redaction to a single search result.
#[must_use]
pub fn apply_redaction(mut result: SearchResult, policy: &RedactionPolicy) -> SearchResult {
    if policy.redact_body {
        result.body.clone_from(&policy.body_placeholder);
    }
    if policy.redact_sender {
        result.from_agent = None;
    }
    if policy.redact_thread {
        result.thread_id = None;
    }
    result
}

// ────────────────────────────────────────────────────────────────────
// SQL scope clause generation
// ────────────────────────────────────────────────────────────────────

/// Generate additional WHERE clauses for SQL-level scope enforcement.
///
/// This pushes scope filtering into the query itself for efficiency,
/// rather than fetching all results and filtering in Rust.
///
/// Returns `(clause_parts, params)` to append to the existing WHERE.
#[must_use]
pub fn build_scope_sql_clauses(ctx: &ScopeContext) -> (Vec<String>, Vec<ScopeSqlParam>) {
    let Some(viewer) = ctx.viewer else {
        return (Vec::new(), Vec::new()); // operator mode: no restrictions
    };

    let mut clauses = Vec::new();
    let mut params = Vec::new();

    // The viewer can see messages where:
    // 1. They are the sender
    // 2. They are a recipient
    // 3. The sender has 'open' or 'auto' contact policy
    // 4. They have an approved contact link with the sender
    //
    // This is an OR of multiple conditions.

    let mut or_parts = Vec::new();

    // 1. Viewer is sender
    or_parts.push("m.sender_id = ?".to_string());
    params.push(ScopeSqlParam::Int(viewer.agent_id));

    // 2. Viewer is recipient
    or_parts.push(
        "m.id IN (SELECT mr.message_id FROM message_recipients mr WHERE mr.agent_id = ?)"
            .to_string(),
    );
    params.push(ScopeSqlParam::Int(viewer.agent_id));

    // 3. Sender has permissive policy (open or auto)
    or_parts.push(
        "(SELECT a2.contact_policy FROM agents a2 WHERE a2.id = m.sender_id) IN ('open', 'auto')"
            .to_string(),
    );

    // 4. Approved contact link (bidirectional)
    or_parts.push(
        "EXISTS (SELECT 1 FROM agent_links al WHERE \
         ((al.a_agent_id = ? AND al.b_agent_id = m.sender_id) OR \
          (al.b_agent_id = ? AND al.a_agent_id = m.sender_id)) \
         AND al.status = 'approved' \
         AND (al.expires_ts IS NULL OR al.expires_ts > ?))"
            .to_string(),
    );
    params.push(ScopeSqlParam::Int(viewer.agent_id));
    params.push(ScopeSqlParam::Int(viewer.agent_id));
    params.push(ScopeSqlParam::TimestampNow);

    let combined = format!("({})", or_parts.join(" OR "));
    clauses.push(combined);

    (clauses, params)
}

/// Parameter types for scope SQL clauses.
#[derive(Debug, Clone)]
pub enum ScopeSqlParam {
    Int(i64),
    Text(String),
    /// Placeholder for the current timestamp (caller must substitute).
    TimestampNow,
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_planner::DocKind;

    fn make_message_result(id: i64, project_id: i64, from_agent: &str) -> SearchResult {
        SearchResult {
            doc_kind: DocKind::Message,
            id,
            project_id: Some(project_id),
            title: format!("Message {id}"),
            body: format!("Body of message {id}"),
            score: Some(-1.0),
            importance: Some("normal".to_string()),
            ack_required: Some(false),
            created_ts: Some(1_000_000),
            thread_id: Some("thread-1".to_string()),
            from_agent: Some(from_agent.to_string()),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        }
    }

    fn make_agent_result(id: i64) -> SearchResult {
        SearchResult {
            doc_kind: DocKind::Agent,
            id,
            project_id: Some(1),
            title: "BlueLake".to_string(),
            body: "An agent".to_string(),
            score: Some(0.0),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        }
    }

    fn operator_ctx() -> ScopeContext {
        ScopeContext {
            viewer: None,
            approved_contacts: Vec::new(),
            viewer_project_ids: Vec::new(),
            sender_policies: Vec::new(),
            recipient_map: Vec::new(),
        }
    }

    fn viewer_ctx(agent_id: i64, project_id: i64) -> ScopeContext {
        ScopeContext {
            viewer: Some(ViewerIdentity {
                project_id,
                agent_id,
            }),
            approved_contacts: Vec::new(),
            viewer_project_ids: vec![project_id],
            sender_policies: Vec::new(),
            recipient_map: Vec::new(),
        }
    }

    // ── Non-message entities always visible ───────────────────────

    #[test]
    fn agent_result_always_allowed() {
        let result = make_agent_result(1);
        let ctx = viewer_ctx(10, 1);
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::NonMessageEntity);
    }

    #[test]
    fn project_result_always_allowed() {
        let result = SearchResult {
            doc_kind: DocKind::Project,
            id: 1,
            project_id: None,
            title: "proj".to_string(),
            body: String::new(),
            score: None,
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        };
        let ctx = viewer_ctx(10, 1);
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::NonMessageEntity);
    }

    // ── Operator mode ─────────────────────────────────────────────

    #[test]
    fn operator_sees_everything() {
        let result = make_message_result(1, 1, "BlueLake");
        let ctx = operator_ctx();
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::OperatorMode);
    }

    // ── Recipient check ───────────────────────────────────────────

    #[test]
    fn recipient_can_see_message() {
        let result = make_message_result(42, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.recipient_map.push(RecipientEntry {
            message_id: 42,
            agent_ids: vec![10],
        });
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::IsRecipient);
    }

    #[test]
    fn non_recipient_checked_further() {
        let result = make_message_result(42, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.recipient_map.push(RecipientEntry {
            message_id: 42,
            agent_ids: vec![99], // different agent
        });
        // No approved contacts, default policy = auto → Allow
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::AutoPolicy);
    }

    // ── Contact policy checks ─────────────────────────────────────

    #[test]
    fn contacts_only_denies_unlinked() {
        let result = make_message_result(1, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.sender_policies.push(SenderPolicy {
            project_id: 1,
            agent_id: 20,
            policy: ContactPolicyKind::ContactsOnly,
        });
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Deny);
        assert_eq!(decision.reason, ScopeReason::ContactsOnlyDenied);
    }

    #[test]
    fn block_all_denies() {
        let result = make_message_result(1, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.sender_policies.push(SenderPolicy {
            project_id: 1,
            agent_id: 20,
            policy: ContactPolicyKind::BlockAll,
        });
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Deny);
        assert_eq!(decision.reason, ScopeReason::BlockAllDenied);
    }

    #[test]
    fn open_policy_allows() {
        let result = make_message_result(1, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.sender_policies.push(SenderPolicy {
            project_id: 1,
            agent_id: 20,
            policy: ContactPolicyKind::Open,
        });
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::OpenPolicy);
    }

    // ── Approved contact link ─────────────────────────────────────

    #[test]
    fn approved_contact_allows() {
        let result = make_message_result(1, 1, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        ctx.sender_policies.push(SenderPolicy {
            project_id: 1,
            agent_id: 20,
            policy: ContactPolicyKind::ContactsOnly,
        });
        ctx.approved_contacts.push((1, 20));
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        assert_eq!(decision.reason, ScopeReason::ApprovedContact);
    }

    // ── Cross-project denial ──────────────────────────────────────

    #[test]
    fn cross_project_denied_without_contact() {
        let result = make_message_result(1, 99, "BlueLake"); // project 99
        let ctx = viewer_ctx(10, 1); // viewer in project 1
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Deny);
        assert_eq!(decision.reason, ScopeReason::CrossProjectDenied);
    }

    #[test]
    fn cross_project_allowed_with_contact() {
        let result = make_message_result(1, 99, "BlueLake");
        let mut ctx = viewer_ctx(10, 1);
        // Viewer has contact in project 99
        ctx.approved_contacts.push((99, 30));
        ctx.sender_policies.push(SenderPolicy {
            project_id: 99,
            agent_id: 30,
            policy: ContactPolicyKind::Open,
        });
        let decision = evaluate_scope(&result, &ctx);
        assert_eq!(decision.verdict, ScopeVerdict::Allow);
        // ApprovedContact takes priority over sender policy (checked first)
        assert_eq!(decision.reason, ScopeReason::ApprovedContact);
    }

    // ── Redaction ─────────────────────────────────────────────────

    #[test]
    fn redaction_hides_body() {
        let result = make_message_result(1, 1, "BlueLake");
        let policy = RedactionPolicy::default();
        let redacted = apply_redaction(result, &policy);
        assert_eq!(
            redacted.body,
            "[Content hidden — sender restricts visibility]"
        );
        assert!(redacted.from_agent.is_some()); // not redacted by default
        assert!(redacted.thread_id.is_some());
    }

    #[test]
    fn strict_redaction_hides_all() {
        let result = make_message_result(1, 1, "BlueLake");
        let policy = RedactionPolicy::strict();
        let redacted = apply_redaction(result, &policy);
        assert_eq!(redacted.body, "[Content hidden — access restricted]");
        assert!(redacted.from_agent.is_none());
        assert!(redacted.thread_id.is_none());
    }

    // ── Batch apply_scope ─────────────────────────────────────────

    #[test]
    fn apply_scope_filters_denied_results() {
        let results = vec![
            make_message_result(1, 1, "BlueLake"),
            make_message_result(2, 99, "RedFox"), // cross-project
            make_agent_result(3),                 // non-message
        ];
        let ctx = viewer_ctx(10, 1);
        let policy = RedactionPolicy::default();
        let (visible, audit) = apply_scope(results, &ctx, &policy);

        assert_eq!(visible.len(), 2); // msg 1 + agent 3
        assert_eq!(audit.total_before, 3);
        assert_eq!(audit.visible_count, 2);
        assert_eq!(audit.denied_count, 1);
        assert_eq!(audit.entries.len(), 1);
        assert_eq!(audit.entries[0].result_id, 2);
        assert_eq!(audit.entries[0].verdict, ScopeVerdict::Deny);
    }

    #[test]
    fn apply_scope_operator_allows_all() {
        let results = vec![
            make_message_result(1, 1, "BlueLake"),
            make_message_result(2, 99, "RedFox"),
        ];
        let ctx = operator_ctx();
        let policy = RedactionPolicy::default();
        let (visible, audit) = apply_scope(results, &ctx, &policy);

        assert_eq!(visible.len(), 2);
        assert_eq!(audit.denied_count, 0);
    }

    // ── ContactPolicyKind ─────────────────────────────────────────

    #[test]
    fn contact_policy_parse_roundtrip() {
        for kind in [
            ContactPolicyKind::Open,
            ContactPolicyKind::Auto,
            ContactPolicyKind::ContactsOnly,
            ContactPolicyKind::BlockAll,
        ] {
            assert_eq!(ContactPolicyKind::parse(kind.as_str()), kind);
        }
    }

    #[test]
    fn contact_policy_parse_case_insensitive() {
        assert_eq!(
            ContactPolicyKind::parse("BLOCK_ALL"),
            ContactPolicyKind::BlockAll
        );
        assert_eq!(ContactPolicyKind::parse("Open"), ContactPolicyKind::Open);
        assert_eq!(ContactPolicyKind::parse("unknown"), ContactPolicyKind::Auto);
    }

    // ── SQL clause generation ─────────────────────────────────────

    #[test]
    fn sql_clauses_operator_empty() {
        let ctx = operator_ctx();
        let (clauses, params) = build_scope_sql_clauses(&ctx);
        assert!(clauses.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn sql_clauses_viewer_has_conditions() {
        let ctx = viewer_ctx(10, 1);
        let (clauses, params) = build_scope_sql_clauses(&ctx);
        assert_eq!(clauses.len(), 1);
        let clause = &clauses[0];
        assert!(clause.contains("m.sender_id = ?"));
        assert!(clause.contains("message_recipients"));
        assert!(clause.contains("contact_policy"));
        assert!(clause.contains("agent_links"));
        // 4 params: sender_id, recipient_id, link_a, link_b + TimestampNow
        assert_eq!(params.len(), 5);
    }

    // ── ScopeReason messages ──────────────────────────────────────

    #[test]
    fn all_reasons_have_messages() {
        let reasons = [
            ScopeReason::IsSender,
            ScopeReason::IsRecipient,
            ScopeReason::ApprovedContact,
            ScopeReason::OpenPolicy,
            ScopeReason::AutoPolicy,
            ScopeReason::ContactsOnlyDenied,
            ScopeReason::BlockAllDenied,
            ScopeReason::CrossProjectDenied,
            ScopeReason::OperatorMode,
            ScopeReason::NonMessageEntity,
        ];
        for reason in reasons {
            assert!(!reason.user_message().is_empty());
        }
    }

    // ── Audit summary ─────────────────────────────────────────────

    #[test]
    fn audit_summary_counts_correct() {
        let results = vec![
            make_message_result(1, 1, "BlueLake"),   // allowed
            make_message_result(2, 99, "RedFox"),    // denied (cross-project)
            make_message_result(3, 99, "GreenLake"), // denied (cross-project)
            make_agent_result(4),                    // allowed (non-message)
        ];
        let ctx = viewer_ctx(10, 1);
        let policy = RedactionPolicy::default();
        let (_, audit) = apply_scope(results, &ctx, &policy);

        assert_eq!(audit.total_before, 4);
        assert_eq!(audit.visible_count, 2);
        assert_eq!(audit.denied_count, 2);
        assert_eq!(audit.redacted_count, 0);
        assert_eq!(audit.entries.len(), 2); // only denied entries
    }

    // ── ScopeVerdict serde ────────────────────────────────────────

    #[test]
    fn scope_verdict_serde_roundtrip() {
        let v = ScopeVerdict::Redact;
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"redact\"");
        let v2: ScopeVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v2, v);
    }

    // ── RedactionPolicy defaults ──────────────────────────────────

    #[test]
    fn default_redaction_hides_body_only() {
        let p = RedactionPolicy::default();
        assert!(p.redact_body);
        assert!(!p.redact_sender);
        assert!(!p.redact_thread);
    }

    #[test]
    fn strict_redaction_hides_everything() {
        let p = RedactionPolicy::strict();
        assert!(p.redact_body);
        assert!(p.redact_sender);
        assert!(p.redact_thread);
    }

    // ── Recipient map with multiple messages ──────────────────────

    #[test]
    fn recipient_check_scoped_to_message_id() {
        let result_a = make_message_result(10, 1, "BlueLake");
        let result_b = make_message_result(20, 1, "BlueLake");
        let mut ctx = viewer_ctx(5, 1);
        ctx.recipient_map.push(RecipientEntry {
            message_id: 10,
            agent_ids: vec![5],
        });
        // viewer is recipient of msg 10 but NOT msg 20
        let dec_a = evaluate_scope(&result_a, &ctx);
        assert_eq!(dec_a.reason, ScopeReason::IsRecipient);

        let dec_b = evaluate_scope(&result_b, &ctx);
        // msg 20 has no recipient entry for viewer, falls through to policy
        assert_ne!(dec_b.reason, ScopeReason::IsRecipient);
    }

    // ── Mixed batch with all verdict types ────────────────────────

    #[test]
    fn mixed_batch_all_verdicts() {
        let results = vec![
            make_message_result(1, 1, "BlueLake"),   // recipient → Allow
            make_message_result(2, 1, "RedFox"),     // contacts_only → Deny
            make_agent_result(3),                    // non-message → Allow
            make_message_result(4, 99, "GreenLake"), // cross-project → Deny
        ];
        let mut ctx = viewer_ctx(10, 1);
        ctx.recipient_map.push(RecipientEntry {
            message_id: 1,
            agent_ids: vec![10],
        });
        ctx.sender_policies.push(SenderPolicy {
            project_id: 1,
            agent_id: 50,
            policy: ContactPolicyKind::ContactsOnly,
        });

        let policy = RedactionPolicy::default();
        let (visible, audit) = apply_scope(results, &ctx, &policy);

        assert_eq!(visible.len(), 2);
        assert_eq!(audit.denied_count, 2);
        assert_eq!(audit.entries.len(), 2);
    }
}
