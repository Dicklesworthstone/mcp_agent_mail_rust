//! Property tests for permission scope evaluation and redaction transforms.
//!
//! Tests the pure functions in `search_scope` without requiring a database.
//! Covers:
//!
//! 1. **Structural invariants** — monotonicity, operator supremacy, count conservation
//! 2. **Adversarial fixtures** — unicode names, alias collisions, i64 extremes,
//!    malformed metadata, self-referential viewers
//! 3. **Redaction irreversibility** — restricted content cannot be reconstructed
//! 4. **Audit completeness** — every denied/redacted result has a corresponding entry
//! 5. **Policy cascade** — exhaustive `ContactPolicyKind` coverage
//! 6. **SQL clause generation** — parameter hygiene
//! 7. **Serde roundtrips** — all enums survive JSON encode/decode

#![allow(
    clippy::similar_names,
    clippy::cast_precision_loss,
    clippy::missing_const_for_fn,
    clippy::match_same_arms
)]

use mcp_agent_mail_db::search_planner::{DocKind, SearchResult};
use mcp_agent_mail_db::search_scope::{
    ContactPolicyKind, RecipientEntry, RedactionPolicy, ScopeAuditSummary, ScopeContext,
    ScopeSqlParam, ScopeVerdict, ScopedSearchResult, SenderPolicy, ViewerIdentity, apply_redaction,
    apply_scope, build_scope_sql_clauses, evaluate_scope,
};

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn msg(id: i64, project_id: i64, from_agent: &str) -> SearchResult {
    SearchResult {
        doc_kind: DocKind::Message,
        id,
        project_id: Some(project_id),
        title: format!("Subject {id}"),
        body: format!("Confidential body #{id}"),
        score: Some(-1.0),
        importance: Some("normal".to_string()),
        ack_required: Some(false),
        created_ts: Some(1_700_000_000_000_000),
        thread_id: Some(format!("thread-{id}")),
        from_agent: Some(from_agent.to_string()),
        redacted: false,
        redaction_reason: None,
    }
}

fn agent_result(id: i64, project_id: i64) -> SearchResult {
    SearchResult {
        doc_kind: DocKind::Agent,
        id,
        project_id: Some(project_id),
        title: "SomeName".to_string(),
        body: "task desc".to_string(),
        score: None,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        redacted: false,
        redaction_reason: None,
    }
}

fn project_result(id: i64) -> SearchResult {
    SearchResult {
        doc_kind: DocKind::Project,
        id,
        project_id: Some(id),
        title: "slug".to_string(),
        body: "/path/to/project".to_string(),
        score: None,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        redacted: false,
        redaction_reason: None,
    }
}

fn operator() -> ScopeContext {
    ScopeContext {
        viewer: None,
        approved_contacts: vec![],
        viewer_project_ids: vec![],
        sender_policies: vec![],
        recipient_map: vec![],
    }
}

fn viewer(agent_id: i64, project_id: i64) -> ScopeContext {
    ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id,
            agent_id,
        }),
        approved_contacts: vec![],
        viewer_project_ids: vec![project_id],
        sender_policies: vec![],
        recipient_map: vec![],
    }
}

/// Verify the audit summary count invariant.
fn assert_audit_invariant(audit: &ScopeAuditSummary, visible: &[ScopedSearchResult]) {
    assert_eq!(
        audit.visible_count + audit.denied_count,
        audit.total_before,
        "visible + denied must equal total_before"
    );
    assert_eq!(
        visible.len(),
        audit.visible_count,
        "visible vec length must match audit.visible_count"
    );
    // Redacted results are a subset of visible
    assert!(
        audit.redacted_count <= audit.visible_count,
        "redacted_count ({}) must be <= visible_count ({})",
        audit.redacted_count,
        audit.visible_count
    );
    // Every denied/redacted result must have an audit entry
    assert_eq!(
        audit.entries.len(),
        audit.denied_count + audit.redacted_count,
        "audit entries must equal denied + redacted"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 1. STRUCTURAL INVARIANTS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn invariant_operator_supremacy() {
    // Operator mode (viewer=None) always yields Allow for ALL doc kinds
    let ctx = operator();
    let policy = RedactionPolicy::default();
    let results = vec![
        msg(1, 1, "A"),
        msg(2, 99, "B"),
        agent_result(3, 1),
        project_result(4),
    ];
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert_eq!(visible.len(), 4);
    assert_eq!(audit.denied_count, 0);
    assert_eq!(audit.redacted_count, 0);
    for sr in &visible {
        assert_eq!(sr.scope.verdict, ScopeVerdict::Allow);
    }
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn invariant_non_message_always_visible() {
    // Agent and Project results are visible regardless of scope restrictions
    let mut ctx = viewer(10, 1);
    // Add a restrictive policy that would deny messages
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::BlockAll,
    });

    let results = vec![
        agent_result(1, 1),
        agent_result(2, 99), // cross-project
        project_result(3),
        agent_result(4, 999), // non-existent project
    ];
    let policy = RedactionPolicy::strict();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert_eq!(visible.len(), 4);
    assert_eq!(audit.denied_count, 0);
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn invariant_count_conservation() {
    // For any batch: visible_count + denied_count == total_before
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();

    // Mix of same-project and cross-project messages
    let results: Vec<SearchResult> = (0..100)
        .map(|i| {
            let project = if i % 3 == 0 { 1 } else { 99 };
            msg(i, project, &format!("Agent{i}"))
        })
        .collect();

    let total = results.len();
    let (visible, audit) = apply_scope(results, &ctx, &policy);
    assert_eq!(audit.total_before, total);
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn invariant_monotonicity_adding_recipient() {
    // Adding the viewer as a recipient can only increase visibility, never reduce it
    let result = msg(42, 1, "Sender");

    // Without recipient entry
    let ctx_without = viewer(10, 1);
    let dec_without = evaluate_scope(&result, &ctx_without);

    // With recipient entry
    let mut ctx_with = viewer(10, 1);
    ctx_with.recipient_map.push(RecipientEntry {
        message_id: 42,
        agent_ids: vec![10],
    });
    let dec_with = evaluate_scope(&result, &ctx_with);

    // Both should allow (auto policy), but with different reasons
    assert_eq!(dec_with.verdict, ScopeVerdict::Allow);
    // The version with recipient should be at least as permissive
    assert!(
        dec_without.verdict == ScopeVerdict::Allow || dec_with.verdict == ScopeVerdict::Allow,
        "adding recipient must not reduce visibility"
    );
}

#[test]
fn invariant_monotonicity_adding_contact() {
    // Adding an approved contact can only increase visibility
    let result = msg(1, 1, "SomeAgent");

    // With contacts_only policy and NO contact link → Deny
    let mut ctx_deny = viewer(10, 1);
    ctx_deny.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 50,
        policy: ContactPolicyKind::ContactsOnly,
    });
    let dec_deny = evaluate_scope(&result, &ctx_deny);
    assert_eq!(dec_deny.verdict, ScopeVerdict::Deny);

    // Same context but WITH contact link → Allow
    let mut ctx_allow = ctx_deny.clone();
    ctx_allow.approved_contacts.push((1, 50));
    let dec_allow = evaluate_scope(&result, &ctx_allow);
    assert_eq!(dec_allow.verdict, ScopeVerdict::Allow);
}

#[test]
fn invariant_monotonicity_adding_project_membership() {
    // Adding the sender's project to viewer_project_ids can only help
    let result = msg(1, 99, "Agent");

    // Viewer only in project 1 → cross-project denied
    let ctx_denied = viewer(10, 1);
    let dec1 = evaluate_scope(&result, &ctx_denied);
    assert_eq!(dec1.verdict, ScopeVerdict::Deny);

    // Viewer in both projects → allowed (auto policy)
    let mut ctx_multi = viewer(10, 1);
    ctx_multi.viewer_project_ids.push(99);
    let dec2 = evaluate_scope(&result, &ctx_multi);
    assert_eq!(dec2.verdict, ScopeVerdict::Allow);
}

// ════════════════════════════════════════════════════════════════════════════
// 2. POLICY CASCADE — exhaustive ContactPolicyKind coverage
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn policy_cascade_open_allows() {
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::Open,
    });
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::OpenPolicy
    );
}

#[test]
fn policy_cascade_auto_allows() {
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::Auto,
    });
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::AutoPolicy
    );
}

#[test]
fn policy_cascade_contacts_only_denies_without_link() {
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::ContactsOnly,
    });
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Deny);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::ContactsOnlyDenied
    );
}

#[test]
fn policy_cascade_block_all_denies() {
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::BlockAll,
    });
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Deny);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::BlockAllDenied
    );
}

#[test]
fn policy_cascade_no_policy_defaults_to_auto() {
    // When no sender_policies entry exists, lookup returns Auto
    let result = msg(1, 1, "UnknownSender");
    let ctx = viewer(10, 1); // no sender_policies at all
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::AutoPolicy
    );
}

#[test]
fn policy_cascade_contact_link_overrides_contacts_only() {
    // Approved contact link should override contacts_only denial
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::ContactsOnly,
    });
    ctx.approved_contacts.push((1, 20));
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::ApprovedContact
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 3. REDACTION IRREVERSIBILITY
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn redaction_body_placeholder_replaces_original() {
    let original_body = "TOP SECRET: launch codes 12345".to_string();
    let result = SearchResult {
        body: original_body.clone(),
        ..msg(1, 1, "Agent")
    };
    let policy = RedactionPolicy::default();
    let redacted = apply_redaction(result, &policy);

    assert_ne!(redacted.body, original_body);
    assert!(!redacted.body.contains("TOP SECRET"));
    assert!(!redacted.body.contains("12345"));
    assert!(!redacted.body.contains("launch codes"));
    assert_eq!(
        redacted.body,
        "[Content hidden — sender restricts visibility]"
    );
}

#[test]
fn redaction_strict_removes_sender_and_thread() {
    let result = SearchResult {
        from_agent: Some("SecretAgent".to_string()),
        thread_id: Some("secret-thread-42".to_string()),
        body: "classified intel".to_string(),
        ..msg(1, 1, "SecretAgent")
    };
    let policy = RedactionPolicy::strict();
    let redacted = apply_redaction(result, &policy);

    assert!(redacted.from_agent.is_none(), "sender must be redacted");
    assert!(redacted.thread_id.is_none(), "thread must be redacted");
    assert!(!redacted.body.contains("classified"));
    assert!(!redacted.body.contains("intel"));
}

#[test]
fn redaction_preserves_non_redacted_fields() {
    let result = msg(42, 7, "Visible");
    let policy = RedactionPolicy::default(); // only body redacted
    let redacted = apply_redaction(result, &policy);

    // These fields must survive
    assert_eq!(redacted.id, 42);
    assert_eq!(redacted.project_id, Some(7));
    assert_eq!(redacted.doc_kind, DocKind::Message);
    assert_eq!(redacted.title, "Subject 42");
    assert!(redacted.from_agent.is_some()); // not redacted by default
    assert!(redacted.thread_id.is_some()); // not redacted by default
    assert_eq!(redacted.importance, Some("normal".to_string()));
}

#[test]
fn redaction_all_8_combinations() {
    // Enumerate all 2^3 = 8 combinations of (redact_body, redact_sender, redact_thread)
    for bits in 0u8..8 {
        let policy = RedactionPolicy {
            redact_body: bits & 1 != 0,
            redact_sender: bits & 2 != 0,
            redact_thread: bits & 4 != 0,
            body_placeholder: "[HIDDEN]".to_string(),
        };
        let result = SearchResult {
            body: "secret".to_string(),
            from_agent: Some("Agent007".to_string()),
            thread_id: Some("thread-x".to_string()),
            ..msg(1, 1, "Agent007")
        };
        let redacted = apply_redaction(result, &policy);

        if policy.redact_body {
            assert_eq!(redacted.body, "[HIDDEN]", "bits={bits}: body not redacted");
        } else {
            assert_eq!(
                redacted.body, "secret",
                "bits={bits}: body wrongly redacted"
            );
        }
        if policy.redact_sender {
            assert!(
                redacted.from_agent.is_none(),
                "bits={bits}: sender not redacted"
            );
        } else {
            assert_eq!(
                redacted.from_agent.as_deref(),
                Some("Agent007"),
                "bits={bits}: sender wrongly redacted"
            );
        }
        if policy.redact_thread {
            assert!(
                redacted.thread_id.is_none(),
                "bits={bits}: thread not redacted"
            );
        } else {
            assert_eq!(
                redacted.thread_id.as_deref(),
                Some("thread-x"),
                "bits={bits}: thread wrongly redacted"
            );
        }
    }
}

#[test]
fn redaction_unicode_body_fully_replaced() {
    let result = SearchResult {
        body: "\u{1F512} \u{0410}\u{0411}\u{0412} \u{4e2d}\u{6587} \u{2603}\u{FE0F}".to_string(),
        ..msg(1, 1, "Agent")
    };
    let policy = RedactionPolicy::default();
    let redacted = apply_redaction(result, &policy);

    // No unicode from original body should survive
    assert!(!redacted.body.contains('\u{1F512}'));
    assert!(!redacted.body.contains('\u{0410}'));
    assert!(!redacted.body.contains('\u{4e2d}'));
    assert!(!redacted.body.contains('\u{2603}'));
}

#[test]
fn redaction_empty_body_gets_placeholder() {
    let result = SearchResult {
        body: String::new(),
        ..msg(1, 1, "Agent")
    };
    let policy = RedactionPolicy::default();
    let redacted = apply_redaction(result, &policy);
    assert_eq!(
        redacted.body,
        "[Content hidden — sender restricts visibility]"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 4. AUDIT COMPLETENESS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn audit_every_denied_has_entry() {
    let results = (0..20)
        .map(|i| {
            let project = if i % 2 == 0 { 1 } else { 99 };
            msg(i, project, &format!("Agent{i}"))
        })
        .collect::<Vec<_>>();

    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert_audit_invariant(&audit, &visible);

    // Verify each audit entry has a valid verdict
    for entry in &audit.entries {
        assert!(
            entry.verdict == ScopeVerdict::Deny || entry.verdict == ScopeVerdict::Redact,
            "audit entry must be deny or redact, got {:?}",
            entry.verdict
        );
        assert!(
            !entry.explanation.is_empty(),
            "audit explanation must not be empty"
        );
        assert!(
            !entry.doc_kind.is_empty(),
            "audit doc_kind must not be empty"
        );
    }

    // Denied result IDs must not appear in visible results
    let denied_ids: Vec<i64> = audit
        .entries
        .iter()
        .filter(|e| e.verdict == ScopeVerdict::Deny)
        .map(|e| e.result_id)
        .collect();
    for sr in &visible {
        assert!(
            !denied_ids.contains(&sr.result.id),
            "denied result {} should not appear in visible set",
            sr.result.id
        );
    }
}

#[test]
fn audit_viewer_identity_present_when_scoped() {
    let results = vec![msg(1, 99, "Agent")]; // cross-project → denied
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (_, audit) = apply_scope(results, &ctx, &policy);

    assert_eq!(audit.entries.len(), 1);
    let entry = &audit.entries[0];
    assert!(
        entry.viewer.is_some(),
        "viewer identity must be recorded in audit"
    );
    assert_eq!(entry.viewer.unwrap().agent_id, 10);
    assert_eq!(entry.viewer.unwrap().project_id, 1);
}

#[test]
fn audit_no_entries_for_operator_mode() {
    let results = vec![msg(1, 1, "A"), msg(2, 99, "B")];
    let ctx = operator();
    let policy = RedactionPolicy::default();
    let (_, audit) = apply_scope(results, &ctx, &policy);

    assert!(
        audit.entries.is_empty(),
        "operator mode should produce no audit entries"
    );
    assert_eq!(audit.denied_count, 0);
    assert_eq!(audit.redacted_count, 0);
}

// ════════════════════════════════════════════════════════════════════════════
// 5. ADVERSARIAL FIXTURES
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn adversarial_unicode_agent_names() {
    let names = [
        "\u{0000}",                                 // null byte
        "\u{202E}ReverseText",                      // RTL override
        "Agent\u{200B}Name",                        // zero-width space
        "\u{1F600}\u{1F600}\u{1F600}",              // emoji spam
        "\u{0410}\u{0433}\u{0435}\u{043D}\u{0442}", // Cyrillic "Agent"
        "a\u{0308}",                                // combining diaeresis
        &"A".repeat(10_000),                        // extremely long name
        "",                                         // empty
        " ",                                        // whitespace only
    ];

    let ctx = viewer(10, 1);
    for name in &names {
        let result = msg(1, 1, name);
        // Must not panic
        let dec = evaluate_scope(&result, &ctx);
        // Should produce a valid verdict
        assert!(
            matches!(
                dec.verdict,
                ScopeVerdict::Allow | ScopeVerdict::Redact | ScopeVerdict::Deny
            ),
            "invalid verdict for name {:?}",
            &name[..name.len().min(20)]
        );
    }
}

#[test]
fn adversarial_extreme_ids() {
    let ids = [0, -1, i64::MIN, i64::MAX, 1, -999_999];
    let ctx = viewer(10, 1);

    for &id in &ids {
        let result = SearchResult {
            id,
            project_id: Some(id),
            ..msg(0, 1, "Agent")
        };
        // Must not panic
        let dec = evaluate_scope(&result, &ctx);
        assert!(
            matches!(
                dec.verdict,
                ScopeVerdict::Allow | ScopeVerdict::Redact | ScopeVerdict::Deny
            ),
            "invalid verdict for id={id}"
        );
    }
}

#[test]
fn adversarial_missing_project_id() {
    let result = SearchResult {
        project_id: None,
        ..msg(1, 1, "Agent")
    };
    let ctx = viewer(10, 1);
    // project_id=None defaults to 0 in scope evaluation; viewer is not in project 0
    let dec = evaluate_scope(&result, &ctx);
    // Should not panic, should deny (cross-project, project 0 not in viewer_project_ids)
    assert_eq!(dec.verdict, ScopeVerdict::Deny);
}

#[test]
fn adversarial_missing_from_agent() {
    let result = SearchResult {
        from_agent: None,
        ..msg(1, 1, "Agent")
    };
    let ctx = viewer(10, 1);
    // from_agent=None: sender check fails, but auto policy allows
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
}

#[test]
fn adversarial_alias_collision_different_projects() {
    // Two agents named "BlueLake" in different projects
    let result_p1 = msg(1, 1, "BlueLake");
    let result_p2 = msg(2, 2, "BlueLake");

    let mut ctx = viewer(10, 1);
    ctx.viewer_project_ids = vec![1, 2]; // viewer in both projects

    // Different policies per project
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::Open,
    });
    ctx.sender_policies.push(SenderPolicy {
        project_id: 2,
        agent_id: 30,
        policy: ContactPolicyKind::BlockAll,
    });

    let dec1 = evaluate_scope(&result_p1, &ctx);
    let dec2 = evaluate_scope(&result_p2, &ctx);

    // Project 1 has Open → Allow; Project 2 has BlockAll → Deny
    assert_eq!(dec1.verdict, ScopeVerdict::Allow);
    assert_eq!(dec2.verdict, ScopeVerdict::Deny);
}

#[test]
fn adversarial_self_referential_viewer() {
    // Viewer is both the sender and recipient of their own message
    let result = msg(42, 1, "SelfAgent");
    let mut ctx = viewer(10, 1);
    ctx.recipient_map.push(RecipientEntry {
        message_id: 42,
        agent_ids: vec![10],
    });
    // Viewer is also in sender_policies (they sent the message)
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 10,
        policy: ContactPolicyKind::BlockAll,
    });

    let dec = evaluate_scope(&result, &ctx);
    // Recipient check triggers first (before policy check)
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );
}

#[test]
fn adversarial_many_recipients_one_message() {
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(50, 1);
    ctx.recipient_map.push(RecipientEntry {
        message_id: 1,
        agent_ids: (1..=100).collect(), // 100 recipients
    });

    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );
}

#[test]
fn adversarial_many_messages_one_recipient() {
    let mut ctx = viewer(10, 1);
    for msg_id in 0..500 {
        ctx.recipient_map.push(RecipientEntry {
            message_id: msg_id,
            agent_ids: vec![10],
        });
    }

    // All 500 messages should be allowed as recipient
    for msg_id in 0..500 {
        let result = msg(msg_id, 1, "Sender");
        let dec = evaluate_scope(&result, &ctx);
        assert_eq!(
            dec.verdict,
            ScopeVerdict::Allow,
            "msg {msg_id} should be allowed"
        );
    }
}

#[test]
fn adversarial_empty_context() {
    // Completely empty context (no projects, no contacts, no policies, no recipients)
    let ctx = ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id: 1,
            agent_id: 10,
        }),
        approved_contacts: vec![],
        viewer_project_ids: vec![], // viewer not in ANY project
        sender_policies: vec![],
        recipient_map: vec![],
    };

    let result = msg(1, 1, "Agent");
    let dec = evaluate_scope(&result, &ctx);
    // Viewer is not in project 1 (empty viewer_project_ids) → cross-project denied
    assert_eq!(dec.verdict, ScopeVerdict::Deny);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::CrossProjectDenied
    );
}

#[test]
fn adversarial_zero_project_id() {
    // project_id = 0 is what None defaults to
    let result = SearchResult {
        project_id: Some(0),
        ..msg(1, 0, "Agent")
    };
    let mut ctx = viewer(10, 1);
    ctx.viewer_project_ids.push(0); // viewer is in "project 0"
    let dec = evaluate_scope(&result, &ctx);
    // Should be allowed via auto policy
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
}

// ════════════════════════════════════════════════════════════════════════════
// 6. CROSS-PROJECT ISOLATION
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn cross_project_isolation_strict() {
    // Messages from projects the viewer is NOT in are denied
    let ctx = viewer(10, 1); // only in project 1
    let policy = RedactionPolicy::default();

    let results = vec![
        msg(1, 1, "A"), // same project
        msg(2, 2, "B"), // different project
        msg(3, 3, "C"), // different project
        msg(4, 1, "D"), // same project
    ];
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert_eq!(visible.len(), 2); // only project 1 messages
    assert_eq!(audit.denied_count, 2);
    assert_audit_invariant(&audit, &visible);

    // Verify the visible ones are from project 1
    for sr in &visible {
        assert_eq!(sr.result.project_id, Some(1));
    }
}

#[test]
fn cross_project_allowed_via_contact_link() {
    let mut ctx = viewer(10, 1);
    ctx.approved_contacts.push((2, 30)); // contact in project 2
    ctx.sender_policies.push(SenderPolicy {
        project_id: 2,
        agent_id: 30,
        policy: ContactPolicyKind::Open,
    });

    let result = msg(1, 2, "Foreign");
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
}

#[test]
fn cross_project_multiple_projects() {
    let mut ctx = viewer(10, 1);
    ctx.viewer_project_ids = vec![1, 5, 10];

    for pid in [1, 5, 10] {
        let result = msg(pid, pid, "Agent");
        let dec = evaluate_scope(&result, &ctx);
        assert_eq!(
            dec.verdict,
            ScopeVerdict::Allow,
            "project {pid} should be allowed"
        );
    }
    for pid in [2_i64, 6, 11, 99] {
        let result = msg(pid, pid, "Agent");
        let dec = evaluate_scope(&result, &ctx);
        assert_eq!(
            dec.verdict,
            ScopeVerdict::Deny,
            "project {pid} should be denied"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 7. RECIPIENT-SCOPED VISIBILITY
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn recipient_check_is_message_id_scoped() {
    let mut ctx = viewer(10, 1);
    ctx.recipient_map.push(RecipientEntry {
        message_id: 5,
        agent_ids: vec![10],
    });

    // Message 5: viewer IS recipient
    let result5 = msg(5, 1, "Sender");
    let dec5 = evaluate_scope(&result5, &ctx);
    assert_eq!(
        dec5.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );

    // Message 6: viewer is NOT recipient (falls through to policy)
    let result6 = msg(6, 1, "Sender");
    let dec6 = evaluate_scope(&result6, &ctx);
    assert_ne!(
        dec6.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );
}

#[test]
fn recipient_multiple_agents_on_same_message() {
    let mut ctx = viewer(10, 1);
    ctx.recipient_map.push(RecipientEntry {
        message_id: 42,
        agent_ids: vec![5, 10, 20],
    });

    let result = msg(42, 1, "Sender");
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 8. BATCH SCOPE APPLY
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn batch_mixed_doc_kinds() {
    let results = vec![
        msg(1, 1, "A"),
        agent_result(2, 1),
        project_result(3),
        msg(4, 99, "B"),
        agent_result(5, 99),
    ];
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    // msg 1: allowed (same project, auto policy)
    // agent 2: allowed (non-message)
    // project 3: allowed (non-message)
    // msg 4: denied (cross-project)
    // agent 5: allowed (non-message, even cross-project)
    assert_eq!(visible.len(), 4);
    assert_eq!(audit.denied_count, 1);
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn batch_empty_input() {
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(vec![], &ctx, &policy);

    assert!(visible.is_empty());
    assert_eq!(audit.total_before, 0);
    assert_eq!(audit.visible_count, 0);
    assert_eq!(audit.denied_count, 0);
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn batch_all_denied() {
    let results = (0..10).map(|i| msg(i, 99, "Agent")).collect();
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert!(visible.is_empty());
    assert_eq!(audit.denied_count, 10);
    assert_audit_invariant(&audit, &visible);
}

#[test]
fn batch_all_allowed() {
    let results = (0..10).map(|i| msg(i, 1, "Agent")).collect();
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert_eq!(visible.len(), 10);
    assert_eq!(audit.denied_count, 0);
    assert_audit_invariant(&audit, &visible);
}

// ════════════════════════════════════════════════════════════════════════════
// 9. SQL CLAUSE GENERATION
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn sql_clauses_operator_produces_nothing() {
    let ctx = operator();
    let (clauses, params) = build_scope_sql_clauses(&ctx);
    assert!(clauses.is_empty());
    assert!(params.is_empty());
}

#[test]
fn sql_clauses_viewer_produces_or_conditions() {
    let ctx = viewer(10, 1);
    let (clauses, params) = build_scope_sql_clauses(&ctx);

    assert_eq!(clauses.len(), 1);
    let clause = &clauses[0];
    // Should have OR conditions for sender, recipient, policy, contacts
    assert!(clause.contains(" OR "), "clause must contain OR");
    assert!(clause.contains("m.sender_id"), "must check sender");
    assert!(
        clause.contains("message_recipients"),
        "must check recipients"
    );
    assert!(clause.contains("contact_policy"), "must check policy");
    assert!(clause.contains("agent_links"), "must check contacts");

    // Verify param types
    assert!(!params.is_empty());
    let has_int = params.iter().any(|p| matches!(p, ScopeSqlParam::Int(_)));
    let has_ts = params
        .iter()
        .any(|p| matches!(p, ScopeSqlParam::TimestampNow));
    assert!(has_int, "must have Int params");
    assert!(has_ts, "must have TimestampNow param");
}

#[test]
fn sql_clauses_viewer_id_correct() {
    let ctx = viewer(42, 7);
    let (_, params) = build_scope_sql_clauses(&ctx);

    // First param should be the viewer's agent_id for sender check
    match &params[0] {
        ScopeSqlParam::Int(v) => assert_eq!(*v, 42),
        other => panic!("expected Int(42), got {other:?}"),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 10. CONTACT POLICY KIND
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn contact_policy_parse_roundtrip_all() {
    let variants = [
        ContactPolicyKind::Open,
        ContactPolicyKind::Auto,
        ContactPolicyKind::ContactsOnly,
        ContactPolicyKind::BlockAll,
    ];
    for kind in variants {
        let s = kind.as_str();
        let parsed = ContactPolicyKind::parse(s);
        assert_eq!(parsed, kind, "roundtrip failed for {s}");
    }
}

#[test]
fn contact_policy_parse_case_insensitive_exhaustive() {
    let cases = [
        ("open", ContactPolicyKind::Open),
        ("OPEN", ContactPolicyKind::Open),
        ("Open", ContactPolicyKind::Open),
        ("oPeN", ContactPolicyKind::Open),
        ("auto", ContactPolicyKind::Auto),
        ("AUTO", ContactPolicyKind::Auto),
        ("contacts_only", ContactPolicyKind::ContactsOnly),
        ("CONTACTS_ONLY", ContactPolicyKind::ContactsOnly),
        ("Contacts_Only", ContactPolicyKind::ContactsOnly),
        ("block_all", ContactPolicyKind::BlockAll),
        ("BLOCK_ALL", ContactPolicyKind::BlockAll),
        ("Block_All", ContactPolicyKind::BlockAll),
    ];
    for (input, expected) in cases {
        assert_eq!(
            ContactPolicyKind::parse(input),
            expected,
            "parse({input:?}) failed"
        );
    }
}

#[test]
fn contact_policy_parse_unknown_defaults_auto() {
    let unknowns = [
        "",
        "none",
        "deny",
        "allow_all",
        "restricted",
        "contacts-only", // hyphen instead of underscore
        "blockall",      // missing underscore
        "CONTACT_ONLY",  // singular
        "\u{0000}",
        "open\0",
    ];
    for input in unknowns {
        assert_eq!(
            ContactPolicyKind::parse(input),
            ContactPolicyKind::Auto,
            "parse({input:?}) should default to Auto"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 11. SERDE ROUNDTRIPS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn serde_scope_verdict_roundtrip() {
    for v in [
        ScopeVerdict::Allow,
        ScopeVerdict::Redact,
        ScopeVerdict::Deny,
    ] {
        let json = serde_json::to_string(&v).unwrap();
        let parsed: ScopeVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, v, "roundtrip failed for {v:?}");
    }
}

#[test]
fn serde_scope_verdict_snake_case() {
    assert_eq!(
        serde_json::to_string(&ScopeVerdict::Allow).unwrap(),
        "\"allow\""
    );
    assert_eq!(
        serde_json::to_string(&ScopeVerdict::Redact).unwrap(),
        "\"redact\""
    );
    assert_eq!(
        serde_json::to_string(&ScopeVerdict::Deny).unwrap(),
        "\"deny\""
    );
}

#[test]
fn serde_contact_policy_roundtrip() {
    for kind in [
        ContactPolicyKind::Open,
        ContactPolicyKind::Auto,
        ContactPolicyKind::ContactsOnly,
        ContactPolicyKind::BlockAll,
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        let parsed: ContactPolicyKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, kind, "roundtrip failed for {kind:?}");
    }
}

#[test]
fn serde_scope_context_roundtrip() {
    let ctx = ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id: 1,
            agent_id: 10,
        }),
        approved_contacts: vec![(1, 20), (2, 30)],
        viewer_project_ids: vec![1, 2],
        sender_policies: vec![SenderPolicy {
            project_id: 1,
            agent_id: 20,
            policy: ContactPolicyKind::Open,
        }],
        recipient_map: vec![RecipientEntry {
            message_id: 42,
            agent_ids: vec![10, 20],
        }],
    };
    let json = serde_json::to_string(&ctx).unwrap();
    let parsed: ScopeContext = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.viewer.unwrap().agent_id, 10);
    assert_eq!(parsed.approved_contacts.len(), 2);
    assert_eq!(parsed.sender_policies.len(), 1);
    assert_eq!(parsed.recipient_map.len(), 1);
}

#[test]
fn serde_redaction_policy_roundtrip() {
    let policy = RedactionPolicy::strict();
    let json = serde_json::to_string(&policy).unwrap();
    let parsed: RedactionPolicy = serde_json::from_str(&json).unwrap();
    assert!(parsed.redact_body);
    assert!(parsed.redact_sender);
    assert!(parsed.redact_thread);
}

#[test]
fn serde_scope_reason_roundtrip() {
    use mcp_agent_mail_db::search_scope::ScopeReason;
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
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: ScopeReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason, "roundtrip failed for {reason:?}");
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 12. SCOPE REASON MESSAGES
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn all_scope_reasons_have_nonempty_messages() {
    use mcp_agent_mail_db::search_scope::ScopeReason;
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
        let msg = reason.user_message();
        assert!(!msg.is_empty(), "{reason:?} has empty message");
        assert!(
            msg.ends_with('.'),
            "{reason:?} message should end with period: {msg}"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 13. PRIORITY / EVALUATION ORDER
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn evaluation_priority_recipient_before_policy() {
    // Even with BlockAll policy, a recipient should still see the message
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.recipient_map.push(RecipientEntry {
        message_id: 1,
        agent_ids: vec![10],
    });
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::BlockAll,
    });

    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::IsRecipient
    );
}

#[test]
fn evaluation_priority_non_message_before_all() {
    // Non-message entities bypass all checks including cross-project
    let result = agent_result(1, 999);
    let ctx = viewer(10, 1); // not in project 999
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::NonMessageEntity
    );
}

#[test]
fn evaluation_priority_operator_before_all_checks() {
    // Operator mode bypasses everything
    let result = msg(1, 999, "Unknown");
    let ctx = operator();
    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::OperatorMode
    );
}

#[test]
fn evaluation_priority_contact_before_policy() {
    // Approved contact should take priority over sender policy lookup
    let result = msg(1, 1, "Sender");
    let mut ctx = viewer(10, 1);
    ctx.approved_contacts.push((1, 20));
    ctx.sender_policies.push(SenderPolicy {
        project_id: 1,
        agent_id: 20,
        policy: ContactPolicyKind::BlockAll, // would deny without contact
    });

    let dec = evaluate_scope(&result, &ctx);
    assert_eq!(dec.verdict, ScopeVerdict::Allow);
    assert_eq!(
        dec.reason,
        mcp_agent_mail_db::search_scope::ScopeReason::ApprovedContact
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 14. SCOPED SEARCH RESULT STRUCTURE
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn scoped_result_carries_decision() {
    let results = vec![msg(1, 1, "Agent")];
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, _) = apply_scope(results, &ctx, &policy);

    assert_eq!(visible.len(), 1);
    let sr = &visible[0];
    assert_eq!(sr.scope.verdict, ScopeVerdict::Allow);
    assert!(sr.redaction_note.is_none());
    assert_eq!(sr.result.id, 1);
}

#[test]
fn scoped_result_denied_not_in_output() {
    let results = vec![msg(1, 99, "Agent")]; // cross-project
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (visible, audit) = apply_scope(results, &ctx, &policy);

    assert!(visible.is_empty());
    assert_eq!(audit.denied_count, 1);
}

// ════════════════════════════════════════════════════════════════════════════
// 15. LARGE BATCH STRESS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn stress_large_batch_performance() {
    // 10,000 results with mixed visibility
    let results: Vec<SearchResult> = (0..10_000)
        .map(|i| {
            let project = match i % 4 {
                0 => 1,
                1 => 2,
                2 => 1,
                _ => 99,
            };
            let kind = match i % 5 {
                0 => DocKind::Agent,
                1 => DocKind::Project,
                _ => DocKind::Message,
            };
            SearchResult {
                doc_kind: kind,
                id: i,
                project_id: Some(project),
                title: format!("item-{i}"),
                body: format!("body-{i}"),
                score: Some(-(i as f64)),
                importance: Some("normal".to_string()),
                ack_required: Some(false),
                created_ts: Some(i * 1_000_000),
                thread_id: Some(format!("t-{}", i % 100)),
                from_agent: Some(format!("Agent{}", i % 50)),
                redacted: false,
                redaction_reason: None,
            }
        })
        .collect();

    let mut ctx = viewer(10, 1);
    ctx.viewer_project_ids = vec![1, 2];

    let policy = RedactionPolicy::default();
    let start = std::time::Instant::now();
    let (visible, audit) = apply_scope(results, &ctx, &policy);
    let elapsed = start.elapsed();

    assert_audit_invariant(&audit, &visible);
    assert!(
        elapsed.as_millis() < 500,
        "10k batch took {elapsed:?}, expected <500ms"
    );
    assert!(!visible.is_empty(), "should have some visible results");
    assert!(audit.denied_count > 0, "should have some denied results");
}

// ════════════════════════════════════════════════════════════════════════════
// 16. REDACTION POLICY VARIANTS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn redaction_custom_placeholder() {
    let policy = RedactionPolicy {
        redact_body: true,
        redact_sender: false,
        redact_thread: false,
        body_placeholder: "*** REDACTED ***".to_string(),
    };
    let result = msg(1, 1, "Agent");
    let redacted = apply_redaction(result, &policy);
    assert_eq!(redacted.body, "*** REDACTED ***");
}

#[test]
fn redaction_empty_placeholder() {
    let policy = RedactionPolicy {
        redact_body: true,
        redact_sender: false,
        redact_thread: false,
        body_placeholder: String::new(),
    };
    let result = msg(1, 1, "Agent");
    let redacted = apply_redaction(result, &policy);
    assert!(redacted.body.is_empty());
}

#[test]
fn redaction_no_op_when_all_false() {
    let policy = RedactionPolicy {
        redact_body: false,
        redact_sender: false,
        redact_thread: false,
        body_placeholder: "should not appear".to_string(),
    };
    let original = msg(1, 1, "Agent");
    let original_body = original.body.clone();
    let original_from = original.from_agent.clone();
    let original_thread = original.thread_id.clone();
    let redacted = apply_redaction(original, &policy);

    assert_eq!(redacted.body, original_body);
    assert_eq!(redacted.from_agent, original_from);
    assert_eq!(redacted.thread_id, original_thread);
}

// ════════════════════════════════════════════════════════════════════════════
// 17. DETERMINISTIC FAILURE TRACE FORMAT
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn audit_entries_are_deterministic() {
    // Running the same input twice should produce identical audit entries
    let results1 = vec![msg(1, 99, "A"), msg(2, 98, "B"), msg(3, 97, "C")];
    let results2 = vec![msg(1, 99, "A"), msg(2, 98, "B"), msg(3, 97, "C")];

    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();

    let (_, audit1) = apply_scope(results1, &ctx, &policy);
    let (_, audit2) = apply_scope(results2, &ctx, &policy);

    assert_eq!(audit1.entries.len(), audit2.entries.len());
    for (e1, e2) in audit1.entries.iter().zip(audit2.entries.iter()) {
        assert_eq!(e1.result_id, e2.result_id);
        assert_eq!(e1.verdict, e2.verdict);
        assert_eq!(e1.reason, e2.reason);
        assert_eq!(e1.explanation, e2.explanation);
    }
}

#[test]
fn audit_entries_serializable_json() {
    let results = vec![msg(1, 99, "Agent")];
    let ctx = viewer(10, 1);
    let policy = RedactionPolicy::default();
    let (_, audit) = apply_scope(results, &ctx, &policy);

    // Full audit summary should serialize to valid JSON
    let json = serde_json::to_string_pretty(&audit).unwrap();
    assert!(json.contains("\"denied_count\""));
    assert!(json.contains("\"entries\""));

    // Should deserialize back
    let parsed: ScopeAuditSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.denied_count, audit.denied_count);
    assert_eq!(parsed.entries.len(), audit.entries.len());
}
