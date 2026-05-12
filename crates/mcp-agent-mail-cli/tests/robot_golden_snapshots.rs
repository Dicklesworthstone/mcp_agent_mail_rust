#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use mcp_agent_mail_cli::robot::{
    AnomalyCard, AttachmentInfo, FacetEntry, MessageContext, OutputFormat, ReservationEntry,
    RobotEnvelope, SearchData, SearchResult, SearchRouteDiagnostic, StatusData,
    SwarmTopologyCoverage, SwarmTopologyEdge, SwarmTopologyHotspot, SwarmTopologyNode,
    SwarmTopologySummary, ThreadMessage, ThreadSummary, format_output, format_output_md,
};
use mcp_agent_mail_db::query_assistance::{AppliedFilterHint, DidYouMeanHint};
use mcp_agent_mail_db::search_planner::{RecoverySuggestion, ZeroResultGuidance};
use serde::Serialize;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-cli")
        .to_path_buf()
}

fn update_goldens_requested() -> bool {
    std::env::var_os("UPDATE_GOLDENS").is_some() || std::env::var_os("UPDATE_GOLDEN").is_some()
}

fn assert_golden(rel_path: &str, actual: &str) {
    let path = repo_root().join("tests/golden/cli").join(rel_path);
    if update_goldens_requested() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create golden fixture directory");
        }
        std::fs::write(&path, actual).expect("update golden fixture");
        eprintln!("updated golden fixture: {}", path.display());
        return;
    }

    let mut expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read golden fixture {}: {err}", path.display()));
    if expected.ends_with('\n') && !actual.ends_with('\n') {
        expected.pop();
    }
    assert_eq!(
        expected,
        actual,
        "golden fixture mismatch for {}; rerun with UPDATE_GOLDENS=1",
        path.display()
    );
}

fn status_envelope() -> RobotEnvelope<StatusData> {
    let mut env = RobotEnvelope::new(
        "robot status",
        OutputFormat::Json,
        StatusData {
            health: "ok".to_string(),
            unread: 2,
            urgent: 1,
            ack_required: 1,
            ack_overdue: 0,
            active_reservations: 1,
            reservations_expiring_soon: 0,
            active_agents: 2,
            recent_messages: 3,
            my_reservations: vec![ReservationEntry {
                agent: Some("RedFox".to_string()),
                path: "crates/mcp-agent-mail-cli/src/**".to_string(),
                exclusive: true,
                remaining_seconds: 3600,
                remaining: Some("1h".to_string()),
                granted_at: Some("2026-01-02T03:00:00Z".to_string()),
            }],
            top_threads: vec![ThreadSummary {
                id: "br-robot-golden".to_string(),
                subject: "Freeze robot output".to_string(),
                participants: 2,
                messages: 3,
                last_activity: "2026-01-02T03:04:00Z".to_string(),
            }],
            anomalies: Vec::new(),
            recommendations: Vec::new(),
            reservation_forecast: None,
            recovery: None,
        },
    )
    .with_alert(
        "warn",
        "One reservation expires soon",
        Some("am robot reservations --expiring=30".to_string()),
    )
    .with_action("am robot inbox --urgent");
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

fn message_envelope() -> RobotEnvelope<MessageContext> {
    let mut env = RobotEnvelope::new(
        "robot message 101",
        OutputFormat::Markdown,
        MessageContext {
            id: 101,
            from: "BlueLake".to_string(),
            from_program: Some("claude-code".to_string()),
            from_model: Some("opus-4.6".to_string()),
            to: vec!["RedFox".to_string()],
            subject: "Robot golden fixture".to_string(),
            body: "Fixture body used to catch markdown drift.".to_string(),
            thread: "br-robot-golden".to_string(),
            position: 2,
            total_in_thread: 3,
            importance: "high".to_string(),
            ack_status: "required".to_string(),
            created: "2026-01-02T03:02:00Z".to_string(),
            age: "2m".to_string(),
            previous: Some("100".to_string()),
            next: Some("102".to_string()),
            attachments: vec![AttachmentInfo {
                name: "audit-notes.txt".to_string(),
                size: "128 B".to_string(),
                mime_type: "text/plain".to_string(),
            }],
        },
    );
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

fn thread_envelope() -> RobotEnvelope<Vec<ThreadMessage>> {
    let mut env = RobotEnvelope::new(
        "robot thread br-robot-golden",
        OutputFormat::Markdown,
        vec![
            ThreadMessage {
                position: 1,
                from: "BlueLake".to_string(),
                to: "RedFox".to_string(),
                age: "3m".to_string(),
                importance: "normal".to_string(),
                ack: "none".to_string(),
                subject: "Start robot golden thread".to_string(),
                body: Some("Opening note for the golden thread.".to_string()),
            },
            ThreadMessage {
                position: 2,
                from: "RedFox".to_string(),
                to: "BlueLake".to_string(),
                age: "1m".to_string(),
                importance: "high".to_string(),
                ack: "required".to_string(),
                subject: "Re: Start robot golden thread".to_string(),
                body: Some("Reply body that should remain stable.".to_string()),
            },
        ],
    );
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

#[derive(Serialize)]
struct AnalyticsGoldenData {
    anomaly_count: usize,
    anomalies: Vec<AnomalyCard>,
    topology: SwarmTopologySummary,
}

fn analytics_envelope() -> RobotEnvelope<AnalyticsGoldenData> {
    let mut env = RobotEnvelope::new(
        "robot analytics",
        OutputFormat::Json,
        AnalyticsGoldenData {
            anomaly_count: 1,
            anomalies: vec![AnomalyCard {
                severity: "warn".to_string(),
                confidence: 0.82,
                category: "reservation_expiry".to_string(),
                headline: "Reservation pressure building".to_string(),
                rationale:
                    "Two agents are converging on the same hot thread and reservation surface."
                        .to_string(),
                remediation: "am robot reservations --conflicts".to_string(),
                playbooks: vec![],
            }],
            topology: SwarmTopologySummary {
                coverage: SwarmTopologyCoverage {
                    agents: 2,
                    threads: 1,
                    reservations: 1,
                    build_slots: 1,
                    products: 1,
                    nodes: 6,
                    edges: 5,
                },
                nodes: vec![
                    SwarmTopologyNode {
                        id: "agent:RedFox".to_string(),
                        kind: "agent".to_string(),
                        label: "RedFox".to_string(),
                        weight: 4,
                        heat_score: 8,
                        edge_count: 3,
                    },
                    SwarmTopologyNode {
                        id: "thread:br-scfay".to_string(),
                        kind: "thread".to_string(),
                        label: "br-scfay".to_string(),
                        weight: 3,
                        heat_score: 6,
                        edge_count: 2,
                    },
                ],
                edges: vec![
                    SwarmTopologyEdge {
                        from: "agent:RedFox".to_string(),
                        to: "thread:br-scfay".to_string(),
                        kind: "sent_messages".to_string(),
                        weight: 3,
                        heat_score: 6,
                    },
                    SwarmTopologyEdge {
                        from: "agent:RedFox".to_string(),
                        to: "build_slot:ci-heavy".to_string(),
                        kind: "holds_build_slot".to_string(),
                        weight: 1,
                        heat_score: 4,
                    },
                    SwarmTopologyEdge {
                        from: "agent:BlueLake".to_string(),
                        to: "reservation:crates/mcp-agent-mail-cli/src/**".to_string(),
                        kind: "holds_reservation".to_string(),
                        weight: 1,
                        heat_score: 3,
                    },
                ],
                contention: vec![SwarmTopologyHotspot {
                    id: "agent:RedFox".to_string(),
                    kind: "agent".to_string(),
                    label: "RedFox".to_string(),
                    heat_score: 8,
                    edge_count: 3,
                }],
            },
        },
    )
    .with_alert(
        "warn",
        "Reservation pressure building",
        Some("am robot reservations --conflicts".to_string()),
    )
    .with_action("am robot analytics --format json");
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

#[derive(Serialize)]
struct SearchQueryClassCase {
    class: &'static str,
    data: SearchData,
}

#[derive(Serialize)]
struct SearchQueryClassData {
    query_classes: Vec<SearchQueryClassCase>,
}

fn search_route(
    method: &str,
    normalized_query: Option<&str>,
    facets: &[&str],
) -> SearchRouteDiagnostic {
    SearchRouteDiagnostic {
        method: method.to_string(),
        normalized_query: normalized_query.map(ToString::to_string),
        used_like_fallback: method == "sql_plan",
        facet_count: facets.len(),
        facets_applied: facets.iter().map(ToString::to_string).collect(),
    }
}

fn search_guidance(kind: &str, label: &str, detail: &str) -> ZeroResultGuidance {
    ZeroResultGuidance {
        summary: "No results found. 1 suggestion available to broaden your search.".to_string(),
        suggestions: vec![RecoverySuggestion {
            kind: kind.to_string(),
            label: label.to_string(),
            detail: Some(detail.to_string()),
        }],
    }
}

fn search_result(id: i64, from: &str, subject: &str, thread: &str, snippet: &str) -> SearchResult {
    SearchResult {
        id,
        relevance: 0.91,
        from: from.to_string(),
        subject: subject.to_string(),
        thread: thread.to_string(),
        snippet: snippet.to_string(),
        age: "2m".to_string(),
    }
}

fn search_data(
    query: &str,
    route: SearchRouteDiagnostic,
    results: Vec<SearchResult>,
) -> SearchData {
    let total_results = results.len();
    SearchData {
        query: query.to_string(),
        total_results,
        results,
        route: Some(route),
        assistance: None,
        guidance: None,
        next_cursor: None,
        plan_diagnostic: None,
        by_thread: vec![FacetEntry {
            value: "br-search-golden".to_string(),
            count: total_results,
        }],
        by_agent: vec![FacetEntry {
            value: "BlueLake".to_string(),
            count: total_results,
        }],
        by_importance: vec![FacetEntry {
            value: "normal".to_string(),
            count: total_results,
        }],
    }
}

fn search_query_classes_envelope() -> RobotEnvelope<SearchQueryClassData> {
    let mut malformed_filter = search_data(
        "form:BlueLake rollback",
        search_route(
            "hybrid_v3",
            Some("form:BlueLake rollback"),
            &["engine:hybrid", "project_id"],
        ),
        Vec::new(),
    );
    malformed_filter.assistance = Some(mcp_agent_mail_db::QueryAssistance {
        query_text: "form:BlueLake rollback".to_string(),
        applied_filter_hints: Vec::new(),
        did_you_mean: vec![DidYouMeanHint {
            token: "form:BlueLake".to_string(),
            suggested_field: "from".to_string(),
            value: "BlueLake".to_string(),
        }],
    });
    malformed_filter.guidance = Some(search_guidance(
        "fix_typo",
        "Did you mean \"from:BlueLake\"?",
        "\"form:BlueLake\" is not a recognized field. Try \"from\" instead.",
    ));

    let mut thread_filter = search_data(
        "thread:br-123 migration",
        search_route(
            "hybrid_v3",
            Some("migration"),
            &["engine:hybrid", "project_id", "thread_id"],
        ),
        vec![search_result(
            203,
            "BlueLake",
            "Migration plan",
            "br-123",
            "Schema migration checklist and owner handoff.",
        )],
    );
    thread_filter.assistance = Some(mcp_agent_mail_db::QueryAssistance {
        query_text: "migration".to_string(),
        applied_filter_hints: vec![AppliedFilterHint {
            field: "thread".to_string(),
            value: "br-123".to_string(),
        }],
        did_you_mean: Vec::new(),
    });

    let mut empty_query = search_data("", search_route("empty_query", None, &[]), Vec::new());
    empty_query.by_thread.clear();
    empty_query.by_agent.clear();
    empty_query.by_importance.clear();
    empty_query.guidance = Some(ZeroResultGuidance {
        summary: "No search terms provided. Enter a word, quoted phrase, thread id, or agent name."
            .to_string(),
        suggestions: vec![RecoverySuggestion {
            kind: "enter_search_terms".to_string(),
            label: "Enter search terms".to_string(),
            detail: Some(
                "Examples: `rollback`, `\"build plan\"`, `thread:br-123`, or `from:BlueLake`."
                    .to_string(),
            ),
        }],
    });

    let data = SearchQueryClassData {
        query_classes: vec![
            SearchQueryClassCase {
                class: "empty_query",
                data: empty_query,
            },
            SearchQueryClassCase {
                class: "exact_message_id",
                data: search_data(
                    "id:2048",
                    search_route(
                        "sql_plan",
                        Some("id:2048"),
                        &["engine:lexical", "project_id"],
                    ),
                    vec![search_result(
                        2048,
                        "BlueLake",
                        "Exact message lookup",
                        "br-search-golden",
                        "Result selected by exact message id.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "quoted_phrase",
                data: search_data(
                    "\"build plan\"",
                    search_route(
                        "hybrid_v3",
                        Some("\"build plan\""),
                        &["engine:hybrid", "project_id"],
                    ),
                    vec![search_result(
                        201,
                        "BlueLake",
                        "Build plan",
                        "br-search-golden",
                        "Quoted phrase matched the subject.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "malformed_filter_typo",
                data: malformed_filter,
            },
            SearchQueryClassCase {
                class: "canonical_thread_filter",
                data: thread_filter,
            },
            SearchQueryClassCase {
                class: "importance_filter",
                data: search_data(
                    "urgent deployment",
                    search_route(
                        "hybrid_v3",
                        Some("urgent deployment"),
                        &["engine:hybrid", "project_id", "importance"],
                    ),
                    vec![search_result(
                        205,
                        "BlueLake",
                        "Urgent deployment",
                        "br-search-golden",
                        "Importance-constrained deployment thread.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "since_filter",
                data: search_data(
                    "incident",
                    search_route(
                        "hybrid_v3",
                        Some("incident"),
                        &["engine:hybrid", "project_id", "time_range_min"],
                    ),
                    vec![search_result(
                        206,
                        "BlueLake",
                        "Incident follow-up",
                        "br-search-golden",
                        "Recent incident note after the since boundary.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "broad_natural_language",
                data: search_data(
                    "why did the archive repair run this morning",
                    search_route(
                        "hybrid_v3",
                        Some("why did the archive repair run this morning"),
                        &["engine:hybrid", "project_id"],
                    ),
                    vec![search_result(
                        207,
                        "BlueLake",
                        "Archive repair root cause",
                        "br-search-golden",
                        "Natural-language query routed through hybrid search.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "boolean_query",
                data: search_data(
                    "rollback AND migration",
                    search_route(
                        "lexical_v3",
                        Some("rollback AND migration"),
                        &["engine:lexical", "project_id"],
                    ),
                    vec![search_result(
                        208,
                        "BlueLake",
                        "Rollback migration",
                        "br-search-golden",
                        "Boolean query preserved for lexical matching.",
                    )],
                ),
            },
            SearchQueryClassCase {
                class: "prefix_query",
                data: search_data(
                    "migrat*",
                    search_route(
                        "lexical_v3",
                        Some("migrat*"),
                        &["engine:lexical", "project_id"],
                    ),
                    vec![search_result(
                        209,
                        "BlueLake",
                        "Migration prefix",
                        "br-search-golden",
                        "Prefix query expands to migration-family terms.",
                    )],
                ),
            },
        ],
    };

    let mut env = RobotEnvelope::new("robot search query-classes", OutputFormat::Json, data);
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

#[test]
fn robot_status_json_matches_golden() {
    let actual = format_output(&status_envelope(), OutputFormat::Json).expect("format json");
    assert_golden("robot/status/json.json", &actual);
}

#[test]
fn robot_status_toon_matches_golden() {
    let actual = format_output(&status_envelope(), OutputFormat::Toon).expect("format toon");
    assert_golden("robot/status/toon.toon", &actual);
}

#[test]
fn robot_message_markdown_matches_golden() {
    let actual =
        format_output_md(&message_envelope(), OutputFormat::Markdown).expect("format markdown");
    assert_golden("robot/message/md.md", &actual);
}

#[test]
fn robot_thread_markdown_matches_golden() {
    let actual =
        format_output_md(&thread_envelope(), OutputFormat::Markdown).expect("format markdown");
    assert_golden("robot/thread/md.md", &actual);
}

#[test]
fn robot_analytics_json_matches_golden() {
    let actual = format_output(&analytics_envelope(), OutputFormat::Json).expect("format json");
    assert_golden("robot/analytics/json.json", &actual);
    assert!(
        !actual.contains("Fixture body"),
        "analytics topology golden must stay metadata-only and not include message bodies"
    );
}

#[test]
fn robot_search_query_classes_json_matches_golden() {
    let actual =
        format_output(&search_query_classes_envelope(), OutputFormat::Json).expect("format json");
    assert_golden("robot/search/query-classes.json", &actual);

    let parsed: serde_json::Value = serde_json::from_str(&actual).expect("golden json");
    assert_eq!(
        parsed["query_classes"].as_array().expect("classes").len(),
        10,
        "golden fixture must cover at least ten robot search query classes"
    );
    assert!(
        actual.contains("\"route\""),
        "search golden must freeze route diagnostics"
    );
    assert!(
        actual.contains("\"assistance\""),
        "search golden must freeze query-assistance metadata"
    );
    assert!(
        actual.contains("\"guidance\""),
        "search golden must freeze query-coach guidance"
    );
}
