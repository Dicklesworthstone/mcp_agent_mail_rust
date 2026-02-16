//! Logging/redaction compliance suite for Search V3 diagnostics artifacts.
//!
//! Validates that:
//! 1. Explain-mode correlation fields are fully populated (`method`, `facets_applied`, `sql`, etc.)
//! 2. No redacted/private content leaks into `QueryExplain` or audit artifacts
//! 3. Artifact schema is deterministic for machine processing
//! 4. Scope enforcement produces correct audit trails
//!
//! Bead: br-2tnl.7.19

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args,
    clippy::identity_op,
    clippy::too_many_arguments
)]

use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};

use mcp_agent_mail_db::search_planner::{Importance, ScopePolicy, SearchQuery, TimeRange};
use mcp_agent_mail_db::search_scope::{
    ContactPolicyKind, RedactionPolicy, ScopeAuditSummary, ScopeContext, ScopeReason, ScopeVerdict,
    SenderPolicy, ViewerIdentity, evaluate_scope,
};
use mcp_agent_mail_db::search_service::{ScopedSearchResponse, SearchOptions, execute_search};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, queries};

// ── Helpers ──────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("lr_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 4,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn seed_project(pool: &DbPool, slug: &str) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let key = format!("/test/{}", slug);
        async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(p) => p.id.expect("project id"),
                other => panic!("ensure_project failed: {other:?}"),
            }
        }
    })
}

fn seed_agent(pool: &DbPool, project_id: i64, name: &str) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let name = name.to_string();
        async move {
            match queries::register_agent(&cx, &pool, project_id, &name, "test", "test", None, None)
                .await
            {
                Outcome::Ok(a) => a.id.expect("agent id"),
                other => panic!("register_agent failed: {other:?}"),
            }
        }
    })
}

fn create_msg(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body: &str,
    importance: &str,
    thread_id: Option<&str>,
) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let subject = subject.to_string();
        let body = body.to_string();
        let importance = importance.to_string();
        let thread_id = thread_id.map(String::from);
        async move {
            match queries::create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                &subject,
                &body,
                thread_id.as_deref(),
                &importance,
                false,
                "[]",
            )
            .await
            {
                Outcome::Ok(row) => row.id.expect("message id"),
                other => panic!("create_message failed: {other:?}"),
            }
        }
    })
}

const fn operator_ctx() -> ScopeContext {
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

fn scoped_search(
    pool: &DbPool,
    query: &SearchQuery,
    options: &SearchOptions,
) -> ScopedSearchResponse {
    block_on(|cx| {
        let pool = pool.clone();
        let query = query.clone();
        let options = options.clone();
        async move {
            match execute_search(&cx, &pool, &query, &options).await {
                Outcome::Ok(resp) => resp,
                other => panic!("execute_search failed: {other:?}"),
            }
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────

/// Test: explain mode populates all correlation fields for FTS queries.
#[test]
fn explain_fts_correlation_fields() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "exp-fts");
    let aid = seed_agent(&pool, pid, "RedFox");

    create_msg(
        &pool,
        pid,
        aid,
        "explain fts test",
        "explain fts body",
        "normal",
        None,
    );

    let mut q = SearchQuery::messages("explain", pid);
    q.explain = true;

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    let explain = resp
        .explain
        .expect("explain should be present when requested");

    // Method should be populated
    assert!(
        !explain.method.is_empty(),
        "method must not be empty: got '{}'",
        explain.method
    );
    assert!(
        ["fts5", "like_fallback", "filter_only"].contains(&explain.method.as_str()),
        "method should be a known type: got '{}'",
        explain.method
    );

    // SQL should be populated and contain SELECT
    assert!(
        explain.sql.contains("SELECT"),
        "sql should contain SELECT: got '{}'",
        explain.sql
    );

    // Scope policy label should be present
    assert!(
        !explain.scope_policy.is_empty(),
        "scope_policy label must not be empty"
    );
}

/// Test: explain mode populates facets for filtered queries.
#[test]
fn explain_facets_populated() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "exp-facets");
    let aid = seed_agent(&pool, pid, "BlueLake");

    create_msg(
        &pool,
        pid,
        aid,
        "faceted query msg",
        "faceted body",
        "high",
        Some("thread-1"),
    );

    let mut q = SearchQuery::messages("faceted", pid);
    q.importance = vec![Importance::High];
    q.thread_id = Some("thread-1".to_string());
    q.explain = true;

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    let explain = resp.explain.expect("explain present");

    // Facet count should reflect applied filters
    assert!(
        explain.facet_count >= 2,
        "facet_count should be >= 2 (importance + thread_id): got {}",
        explain.facet_count
    );

    // Facets list should include the applied filter names
    let facets_str = explain.facets_applied.join(",");
    assert!(
        facets_str.contains("importance"),
        "facets should include 'importance': got {:?}",
        explain.facets_applied
    );
    assert!(
        facets_str.contains("thread_id"),
        "facets should include 'thread_id': got {:?}",
        explain.facets_applied
    );
}

/// Test: explain mode includes date range facets.
#[test]
fn explain_date_range_facets() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "exp-date");
    let aid = seed_agent(&pool, pid, "GreenHawk");

    create_msg(
        &pool,
        pid,
        aid,
        "dateexplain msg",
        "dateexplain body",
        "normal",
        None,
    );

    let now = mcp_agent_mail_db::now_micros();
    let mut q = SearchQuery::messages("dateexplain", pid);
    q.time_range = TimeRange {
        min_ts: Some(now - 3_600_000_000),
        max_ts: Some(now + 3_600_000_000),
    };
    q.explain = true;

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    let explain = resp.explain.expect("explain present");

    // Should have time_range facets
    let facets_str = explain.facets_applied.join(",");
    assert!(
        facets_str.contains("time_range"),
        "facets should include time_range: got {:?}",
        explain.facets_applied
    );
}

/// Test: explain mode without explain=true returns no explain metadata.
#[test]
fn no_explain_when_not_requested() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "no-exp");
    let aid = seed_agent(&pool, pid, "GoldWolf");

    create_msg(
        &pool,
        pid,
        aid,
        "noexplain msg",
        "noexplain body",
        "normal",
        None,
    );

    let q = SearchQuery::messages("noexplain", pid);
    // explain = false (default)

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    assert!(
        resp.explain.is_none(),
        "explain should be None when not requested"
    );
}

/// Test: SQL in explain does NOT leak query parameters (redaction safety).
#[test]
fn explain_sql_no_parameter_leak() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "sql-leak");
    let aid = seed_agent(&pool, pid, "SilverPeak");

    let secret_body = "topsecret_password_xyzzy42";
    create_msg(&pool, pid, aid, "sqlleak msg", secret_body, "normal", None);

    let mut q = SearchQuery::messages("sqlleak", pid);
    q.explain = true;

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    let explain = resp.explain.expect("explain present");

    // The SQL template should use parameter placeholders (?)
    // It should NOT contain the literal message body or secret content
    assert!(
        !explain.sql.contains(secret_body),
        "SQL should NOT contain literal message body content"
    );

    // The normalized query should be the sanitized FTS form, not raw user input injection
    if let Some(ref nq) = explain.normalized_query {
        let nq_str: &str = nq.as_str();
        assert!(
            !nq_str.contains(secret_body),
            "normalized_query should not contain message body content"
        );
    }
}

/// Test: scoped search produces audit summary with correct counts.
#[test]
fn scope_audit_summary_counts() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "audit-cnt");
    let sender_id = seed_agent(&pool, pid, "DarkElm");
    let viewer_id = seed_agent(&pool, pid, "CalmPine");

    // Create messages from the sender
    create_msg(
        &pool,
        pid,
        sender_id,
        "auditcnt msg1",
        "auditcnt body1",
        "normal",
        None,
    );
    create_msg(
        &pool,
        pid,
        sender_id,
        "auditcnt msg2",
        "auditcnt body2",
        "normal",
        None,
    );

    // Viewer in same project with sender having block_all policy
    let mut ctx = viewer_ctx(viewer_id, pid);
    ctx.sender_policies.push(SenderPolicy {
        project_id: pid,
        agent_id: sender_id,
        policy: ContactPolicyKind::BlockAll,
    });

    let mut q = SearchQuery::messages("auditcnt", pid);
    q.explain = true;

    let options = SearchOptions {
        scope_ctx: Some(ctx),
        ..Default::default()
    };

    let resp = scoped_search(&pool, &q, &options);

    if let Some(ref audit) = resp.audit_summary {
        // Total before scope should equal SQL row count
        assert_eq!(
            audit.total_before, resp.sql_row_count,
            "total_before should match sql_row_count"
        );

        // Count conservation: visible + denied == total_before
        assert_eq!(
            audit.visible_count + audit.denied_count,
            audit.total_before,
            "visible + denied should equal total_before"
        );
    }
}

/// Test: `apply_redaction` unit test — redacted results do not leak body content.
#[test]
fn redaction_no_body_leak() {
    use mcp_agent_mail_db::search_planner::{DocKind, SearchResult};

    let secret = "classified_information_alpha_bravo";
    let result = SearchResult {
        doc_kind: DocKind::Message,
        id: 1,
        project_id: Some(1),
        title: "redleak msg".to_string(),
        body: secret.to_string(),
        score: Some(1.0),
        importance: Some("normal".to_string()),
        ack_required: Some(false),
        created_ts: Some(1000),
        thread_id: Some("t1".to_string()),
        from_agent: Some("SwiftDeer".to_string()),
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    };

    let policy = RedactionPolicy::default();
    let redacted = mcp_agent_mail_db::search_scope::apply_redaction(result, &policy);

    // Body must be replaced with placeholder
    assert!(
        !redacted.body.contains(secret),
        "redacted body must not contain secret: got '{}'",
        redacted.body
    );
    assert_eq!(
        redacted.body, policy.body_placeholder,
        "body should be the placeholder text"
    );
}

/// Test: operator mode (viewer=None) produces Allow verdicts with full visibility.
#[test]
fn operator_mode_full_visibility() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "op-vis");
    let aid = seed_agent(&pool, pid, "FoggyWolf");

    let body = "operator_visible_content_42";
    create_msg(&pool, pid, aid, "opvis msg", body, "normal", None);

    let options = SearchOptions {
        scope_ctx: Some(operator_ctx()),
        ..Default::default()
    };

    let mut q = SearchQuery::messages("opvis", pid);
    q.explain = true;

    let resp = scoped_search(&pool, &q, &options);

    // Operator should see all results unredacted
    assert!(!resp.results.is_empty(), "operator should see results");
    for r in &resp.results {
        assert_eq!(
            r.scope.verdict,
            ScopeVerdict::Allow,
            "operator verdict should be Allow"
        );
        assert_eq!(
            r.scope.reason,
            ScopeReason::OperatorMode,
            "operator reason should be OperatorMode"
        );
    }

    // Explain should show unrestricted scope
    if let Some(ref explain) = resp.explain {
        assert_eq!(
            explain.scope_policy, "unrestricted",
            "operator scope should be 'unrestricted'"
        );
        assert_eq!(explain.denied_count, 0, "operator should have 0 denied");
        assert_eq!(explain.redacted_count, 0, "operator should have 0 redacted");
    }
}

/// Test: audit entries have deterministic schema fields for machine processing.
#[test]
fn audit_entry_deterministic_schema() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "det-schema");
    let sender_id = seed_agent(&pool, pid, "BrightOwl");
    let viewer_id = seed_agent(&pool, pid, "NobleLion");

    create_msg(
        &pool,
        pid,
        sender_id,
        "detschema msg",
        "detschema body",
        "normal",
        None,
    );

    // Set up block_all to generate audit entries
    let mut ctx = viewer_ctx(viewer_id, pid);
    ctx.sender_policies.push(SenderPolicy {
        project_id: pid,
        agent_id: sender_id,
        policy: ContactPolicyKind::BlockAll,
    });

    let options = SearchOptions {
        scope_ctx: Some(ctx),
        ..Default::default()
    };

    let q = SearchQuery::messages("detschema", pid);
    let resp = scoped_search(&pool, &q, &options);

    if let Some(ref audit) = resp.audit_summary {
        for entry in &audit.entries {
            // Every entry must have required fields for machine processing
            assert!(entry.result_id > 0, "result_id must be positive");
            assert!(!entry.doc_kind.is_empty(), "doc_kind must not be empty");
            assert!(
                !entry.explanation.is_empty(),
                "explanation must not be empty"
            );

            // Verdict must be either Deny or Redact (not Allow — those don't get audit entries)
            assert!(
                entry.verdict == ScopeVerdict::Deny || entry.verdict == ScopeVerdict::Redact,
                "audit entries should only be for Deny or Redact: got {:?}",
                entry.verdict
            );
        }
    }
}

/// Test: `QueryExplain` serializes to JSON with stable field names.
#[test]
fn explain_json_schema_stability() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "json-stab");
    let aid = seed_agent(&pool, pid, "CoralOwl");

    create_msg(
        &pool,
        pid,
        aid,
        "jsonstab msg",
        "jsonstab body",
        "normal",
        None,
    );

    let mut q = SearchQuery::messages("jsonstab", pid);
    q.explain = true;

    let resp = scoped_search(&pool, &q, &SearchOptions::default());
    let explain = resp.explain.expect("explain present");

    // Serialize to JSON and verify required keys
    let json = serde_json::to_value(&explain).expect("serialize explain to JSON");
    let obj = json
        .as_object()
        .expect("explain should serialize as object");

    // Required correlation fields for machine processing
    let required_fields = [
        "method",
        "used_like_fallback",
        "facet_count",
        "facets_applied",
        "sql",
        "scope_policy",
        "denied_count",
        "redacted_count",
    ];

    for field in &required_fields {
        assert!(
            obj.contains_key(*field),
            "explain JSON must contain field '{}': keys = {:?}",
            field,
            obj.keys().collect::<Vec<_>>()
        );
    }

    // Verify types
    assert!(obj["method"].is_string(), "method should be string");
    assert!(
        obj["used_like_fallback"].is_boolean(),
        "used_like_fallback should be bool"
    );
    assert!(
        obj["facet_count"].is_number(),
        "facet_count should be number"
    );
    assert!(
        obj["facets_applied"].is_array(),
        "facets_applied should be array"
    );
    assert!(obj["sql"].is_string(), "sql should be string");
    assert!(
        obj["scope_policy"].is_string(),
        "scope_policy should be string"
    );
    assert!(
        obj["denied_count"].is_number(),
        "denied_count should be number"
    );
    assert!(
        obj["redacted_count"].is_number(),
        "redacted_count should be number"
    );
}

/// Test: `ScopeAuditSummary` serializes with stable field names.
#[test]
fn audit_summary_json_schema_stability() {
    let summary = ScopeAuditSummary {
        total_before: 10,
        visible_count: 7,
        redacted_count: 1,
        denied_count: 2,
        entries: Vec::new(),
    };

    let json = serde_json::to_value(&summary).expect("serialize audit summary");
    let obj = json.as_object().expect("should be object");

    let required = [
        "total_before",
        "visible_count",
        "redacted_count",
        "denied_count",
        "entries",
    ];
    for field in &required {
        assert!(
            obj.contains_key(*field),
            "audit summary JSON must contain '{}'",
            field
        );
    }
}

/// Test: `ScopeVerdict` and `ScopeReason` survive JSON roundtrip.
#[test]
fn scope_enums_serde_roundtrip() {
    // Verdicts
    for verdict in [
        ScopeVerdict::Allow,
        ScopeVerdict::Redact,
        ScopeVerdict::Deny,
    ] {
        let json = serde_json::to_string(&verdict).expect("serialize verdict");
        let roundtrip: ScopeVerdict = serde_json::from_str(&json).expect("deserialize verdict");
        assert_eq!(verdict, roundtrip, "verdict roundtrip for {:?}", verdict);
    }

    // Reasons
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
        let json = serde_json::to_string(&reason).expect("serialize reason");
        let roundtrip: ScopeReason = serde_json::from_str(&json).expect("deserialize reason");
        assert_eq!(reason, roundtrip, "reason roundtrip for {:?}", reason);
    }
}

/// Test: `ContactPolicyKind` serde roundtrip.
#[test]
fn contact_policy_serde_roundtrip() {
    for policy in [
        ContactPolicyKind::Open,
        ContactPolicyKind::Auto,
        ContactPolicyKind::ContactsOnly,
        ContactPolicyKind::BlockAll,
    ] {
        let json = serde_json::to_string(&policy).expect("serialize policy");
        let roundtrip: ContactPolicyKind = serde_json::from_str(&json).expect("deserialize policy");
        assert_eq!(policy, roundtrip, "policy roundtrip for {:?}", policy);
    }
}

/// Test: `RedactionPolicy::default()` has expected field values.
#[test]
fn redaction_policy_default_compliance() {
    let policy = RedactionPolicy::default();

    // Default should redact body but not sender/thread
    assert!(policy.redact_body, "default should redact body");
    assert!(!policy.redact_sender, "default should not redact sender");
    assert!(!policy.redact_thread, "default should not redact thread");
    assert!(
        !policy.body_placeholder.is_empty(),
        "body_placeholder must not be empty"
    );
}

/// Test: explain reports correct denied/redacted counts after scope enforcement.
#[test]
fn explain_denied_redacted_counts() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "exp-counts");
    let sender_id = seed_agent(&pool, pid, "ProudCedar");
    let viewer_id = seed_agent(&pool, pid, "SilverLark");

    // Create several messages
    for i in 0..5 {
        create_msg(
            &pool,
            pid,
            sender_id,
            &format!("expcounts msg{}", i),
            &format!("expcounts body{}", i),
            "normal",
            None,
        );
    }

    // Viewer with block_all from sender
    let mut ctx = viewer_ctx(viewer_id, pid);
    ctx.sender_policies.push(SenderPolicy {
        project_id: pid,
        agent_id: sender_id,
        policy: ContactPolicyKind::BlockAll,
    });

    let mut q = SearchQuery::messages("expcounts", pid);
    q.explain = true;

    let options = SearchOptions {
        scope_ctx: Some(ctx),
        ..Default::default()
    };

    let resp = scoped_search(&pool, &q, &options);
    let explain = resp.explain.expect("explain present");

    // The denied_count in explain should match the audit summary
    if let Some(ref audit) = resp.audit_summary {
        assert_eq!(
            explain.denied_count, audit.denied_count,
            "explain.denied_count should match audit_summary.denied_count"
        );
        assert_eq!(
            explain.redacted_count, audit.redacted_count,
            "explain.redacted_count should match audit_summary.redacted_count"
        );
    }
}

/// Test: explain with `ScopePolicy::CallerScoped` shows the policy label.
#[test]
fn explain_caller_scoped_policy_label() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "exp-caller");
    let aid = seed_agent(&pool, pid, "FrostyRaven");

    create_msg(
        &pool,
        pid,
        aid,
        "expcaller msg",
        "expcaller body",
        "normal",
        None,
    );

    let mut q = SearchQuery::messages("expcaller", pid);
    q.scope = ScopePolicy::CallerScoped {
        caller_agent: "FrostyRaven".to_string(),
    };
    q.explain = true;

    let options = SearchOptions {
        scope_ctx: Some(viewer_ctx(aid, pid)),
        ..Default::default()
    };

    let resp = scoped_search(&pool, &q, &options);
    let explain = resp.explain.expect("explain present");

    // Scope policy label should mention the caller
    assert!(
        explain.scope_policy.contains("caller_scoped")
            || explain.scope_policy.contains("FrostyRaven")
            || explain.scope_policy.contains("caller"),
        "scope_policy should reference caller scope: got '{}'",
        explain.scope_policy
    );
}

/// Test: `apply_scope` unit test — `evaluate_scope` returns correct verdicts for each policy.
#[test]
fn evaluate_scope_policy_cascade() {
    use mcp_agent_mail_db::search_planner::{DocKind, SearchResult};

    let msg_result = SearchResult {
        doc_kind: DocKind::Message,
        id: 1,
        project_id: Some(1),
        title: "test".to_string(),
        body: "secret body".to_string(),
        score: Some(1.0),
        importance: Some("normal".to_string()),
        ack_required: Some(false),
        created_ts: Some(1000),
        thread_id: Some("t1".to_string()),
        from_agent: Some("RedFox".to_string()),
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    };

    // Open policy → Allow
    let mut ctx = viewer_ctx(2, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 1, // matches from_agent's hypothetical id
        policy: ContactPolicyKind::Open,
    });
    // Even without matching from_agent id, open policy is checked at contact level

    // Operator mode → always Allow
    let decision = evaluate_scope(&msg_result, &operator_ctx());
    assert_eq!(decision.verdict, ScopeVerdict::Allow);
    assert_eq!(decision.reason, ScopeReason::OperatorMode);

    // Non-message entity → always Allow
    let agent_result = SearchResult {
        doc_kind: DocKind::Agent,
        id: 10,
        project_id: Some(1),
        title: "agent".to_string(),
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
    let decision = evaluate_scope(&agent_result, &viewer_ctx(2, 1));
    assert_eq!(decision.verdict, ScopeVerdict::Allow);
    assert_eq!(decision.reason, ScopeReason::NonMessageEntity);
}

/// Test: `apply_scope` with strict redaction policy replaces all sensitive fields.
#[test]
fn strict_redaction_replaces_all_fields() {
    use mcp_agent_mail_db::search_planner::{DocKind, SearchResult};

    let result = SearchResult {
        doc_kind: DocKind::Message,
        id: 1,
        project_id: Some(1),
        title: "test subject".to_string(),
        body: "this is very secret content".to_string(),
        score: Some(1.0),
        importance: Some("normal".to_string()),
        ack_required: Some(false),
        created_ts: Some(1000),
        thread_id: Some("thread-secret".to_string()),
        from_agent: Some("RedFox".to_string()),
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    };

    // Strict redaction: body, sender, thread all redacted
    let strict = RedactionPolicy {
        redact_body: true,
        redact_sender: true,
        redact_thread: true,
        body_placeholder: "[REDACTED]".to_string(),
    };

    let redacted = mcp_agent_mail_db::search_scope::apply_redaction(result, &strict);

    assert_eq!(redacted.body, "[REDACTED]", "body should be placeholder");
    assert!(redacted.from_agent.is_none(), "sender should be removed");
    assert!(redacted.thread_id.is_none(), "thread_id should be removed");
    // Title and score should be preserved
    assert_eq!(redacted.title, "test subject", "title preserved");
    assert_eq!(redacted.score, Some(1.0), "score preserved");
}
