//! Bounded query-plan diagnostics for selected hot SQLite paths.
//!
//! This module intentionally runs only on explicit diagnostic surfaces. It
//! never sits on the production query hot path.

use crate::DbConn;
use crate::queries::UNKNOWN_SENDER_DISPLAY;
use crate::search_planner::{PlanParam, SearchQuery, plan_search};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlmodel_core::Value;

const PLAN_HASH_SCHEMA_VERSION: &str = "query-plan-diagnostics:v1";
const DEFAULT_LIMIT: i64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotQueryPath {
    Inbox,
    ProductInbox,
    ActiveReservations,
    SearchFallback,
}

impl HotQueryPath {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::ProductInbox => "product_inbox",
            Self::ActiveReservations => "active_reservations",
            Self::SearchFallback => "search_fallback",
        }
    }
}

#[derive(Debug, Clone)]
struct IndexExpectation {
    label: &'static str,
    alternatives: &'static [&'static str],
}

#[derive(Debug, Clone)]
struct HotQuerySpec {
    path: HotQueryPath,
    sql: String,
    params: Vec<Value>,
    expected_indexes: Vec<IndexExpectation>,
    scan_sensitive_sources: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlanStep {
    pub id: i64,
    pub parent: i64,
    pub notused: i64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlanDiagnostic {
    pub path: String,
    pub status: String,
    pub plan_hash: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_expected_indexes: Vec<String>,
    pub steps: Vec<QueryPlanStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlanSummary {
    pub status: String,
    pub detail: String,
    pub diagnostics: Vec<QueryPlanDiagnostic>,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryPlanError {
    #[error("query-plan explain failed for {path}: {source}")]
    Explain {
        path: &'static str,
        source: sqlmodel_core::Error,
    },
    #[error("query-plan row decode failed for {path}: {detail}")]
    Decode { path: &'static str, detail: String },
}

pub fn summarize_hot_query_plans(conn: &DbConn) -> Result<QueryPlanSummary, QueryPlanError> {
    let diagnostics = default_hot_query_specs()
        .into_iter()
        .map(|spec| explain_query_plan(conn, &spec))
        .collect::<Result<Vec<_>, _>>()?;

    let warning_count = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.status != "ok")
        .count();
    let status = if warning_count == 0 { "ok" } else { "warn" }.to_string();
    let detail = if warning_count == 0 {
        format!(
            "{} hot query plan(s) match expected index shapes",
            diagnostics.len()
        )
    } else {
        format!(
            "{} of {} hot query plan(s) have scan or missing-index warnings",
            warning_count,
            diagnostics.len()
        )
    };

    Ok(QueryPlanSummary {
        status,
        detail,
        diagnostics,
    })
}

pub fn search_fallback_plan_diagnostic(
    conn: &DbConn,
    query: &SearchQuery,
) -> Result<QueryPlanDiagnostic, QueryPlanError> {
    explain_query_plan(conn, &search_fallback_spec(query))
}

fn default_hot_query_specs() -> Vec<HotQuerySpec> {
    vec![
        inbox_spec(),
        product_inbox_spec(),
        active_reservations_spec(),
        search_fallback_spec(&SearchQuery::messages("needle".to_string(), 1)),
    ]
}

fn explain_query_plan(
    conn: &DbConn,
    spec: &HotQuerySpec,
) -> Result<QueryPlanDiagnostic, QueryPlanError> {
    let explain_sql = format!("EXPLAIN QUERY PLAN {}", spec.sql);
    let rows = conn
        .query_sync(&explain_sql, &spec.params)
        .map_err(|source| QueryPlanError::Explain {
            path: spec.path.as_str(),
            source,
        })?;

    let mut steps = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row
            .get_as(0)
            .map_err(|error| decode_error(spec.path, error))?;
        let parent = row
            .get_as(1)
            .map_err(|error| decode_error(spec.path, error))?;
        let notused = row
            .get_as(2)
            .map_err(|error| decode_error(spec.path, error))?;
        let detail = row
            .get_as(3)
            .map_err(|error| decode_error(spec.path, error))?;
        steps.push(QueryPlanStep {
            id,
            parent,
            notused,
            detail,
        });
    }

    let missing_expected_indexes = missing_expected_indexes(&steps, &spec.expected_indexes);
    let warnings = scan_warnings(&steps, spec.scan_sensitive_sources, &spec.expected_indexes);
    let status = if missing_expected_indexes.is_empty() && warnings.is_empty() {
        "ok"
    } else {
        "warn"
    }
    .to_string();
    let detail = if status == "ok" {
        "plan uses expected indexed access".to_string()
    } else {
        let mut parts = Vec::new();
        if !missing_expected_indexes.is_empty() {
            parts.push(format!(
                "missing expected index group(s): {}",
                missing_expected_indexes.join(", ")
            ));
        }
        if !warnings.is_empty() {
            parts.push(format!("scan warning(s): {}", warnings.len()));
        }
        parts.join("; ")
    };

    Ok(QueryPlanDiagnostic {
        path: spec.path.as_str().to_string(),
        status,
        plan_hash: plan_hash(spec.path, &steps),
        detail,
        warnings,
        missing_expected_indexes,
        steps,
    })
}

fn decode_error(path: HotQueryPath, error: sqlmodel_core::Error) -> QueryPlanError {
    QueryPlanError::Decode {
        path: path.as_str(),
        detail: error.to_string(),
    }
}

fn missing_expected_indexes(steps: &[QueryPlanStep], expected: &[IndexExpectation]) -> Vec<String> {
    let details = normalized_details(steps);
    expected
        .iter()
        .filter(|expectation| {
            !expectation.alternatives.iter().any(|fragment| {
                let fragment = fragment.to_ascii_lowercase();
                details.iter().any(|detail| detail.contains(&fragment))
            })
        })
        .map(|expectation| expectation.label.to_string())
        .collect()
}

fn scan_warnings(
    steps: &[QueryPlanStep],
    sensitive_sources: &[&str],
    expected_indexes: &[IndexExpectation],
) -> Vec<String> {
    let sensitive_sources = sensitive_sources
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let expected_index_fragments = expected_indexes
        .iter()
        .flat_map(|expectation| expectation.alternatives.iter())
        .map(|fragment| fragment.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized_details(steps)
        .into_iter()
        .filter_map(|detail| {
            let scanned = sensitive_sources
                .iter()
                .any(|source| detail_scans_source(&detail, source));
            let uses_expected_index = expected_index_fragments
                .iter()
                .any(|fragment| detail.contains(fragment));
            let scan_uses_unexpected_index = !detail.contains("using ") || !uses_expected_index;
            if scanned && scan_uses_unexpected_index {
                Some(detail)
            } else {
                None
            }
        })
        .collect()
}

fn detail_scans_source(detail: &str, source: &str) -> bool {
    let mut previous = "";
    let mut before_previous = "";
    for token in detail.split_whitespace() {
        if previous == "scan" && token == source {
            return true;
        }
        if before_previous == "scan" && previous == "table" && token == source {
            return true;
        }
        before_previous = previous;
        previous = token;
    }
    false
}

fn normalized_details(steps: &[QueryPlanStep]) -> Vec<String> {
    steps
        .iter()
        .map(|step| step.detail.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|detail| detail.to_ascii_lowercase())
        .collect()
}

fn plan_hash(path: HotQueryPath, steps: &[QueryPlanStep]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PLAN_HASH_SCHEMA_VERSION.as_bytes());
    hasher.update(b"\n");
    hasher.update(path.as_str().as_bytes());
    hasher.update(b"\n");
    for detail in normalized_details(steps) {
        hasher.update(detail.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

fn inbox_spec() -> HotQuerySpec {
    HotQuerySpec {
        path: HotQueryPath::Inbox,
        sql: format!(
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, '' AS body_md, \
                    m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                    r.kind, COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, r.read_ts, r.ack_ts \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             LEFT JOIN agents s ON s.id = m.sender_id \
             WHERE r.agent_id = ? AND m.project_id = ? \
             ORDER BY m.created_ts DESC LIMIT ?"
        ),
        params: vec![
            Value::BigInt(1),
            Value::BigInt(1),
            Value::BigInt(DEFAULT_LIMIT),
        ],
        expected_indexes: vec![
            IndexExpectation {
                label: "message_recipients(agent_id)",
                alternatives: &[
                    "idx_message_recipients_agent",
                    "idx_message_recipients_agent_message",
                    "idx_mr_agent_ack",
                ],
            },
            IndexExpectation {
                label: "messages primary key",
                alternatives: &["integer primary key", "primary key"],
            },
        ],
        scan_sensitive_sources: &["r", "message_recipients", "m", "messages"],
    }
}

fn product_inbox_spec() -> HotQuerySpec {
    HotQuerySpec {
        path: HotQueryPath::ProductInbox,
        sql: "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, '' AS body_md, \
                    m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                    r.kind, COALESCE(s.name, ?) AS sender_name, r.read_ts, r.ack_ts \
             FROM product_project_links ppl \
             JOIN agents recipient ON recipient.project_id = ppl.project_id AND recipient.name = ? COLLATE NOCASE \
             JOIN message_recipients r ON r.agent_id = recipient.id \
             JOIN messages m ON m.id = r.message_id AND m.project_id = ppl.project_id \
             LEFT JOIN agents s ON s.id = m.sender_id \
             WHERE ppl.product_id = ? \
             ORDER BY m.created_ts DESC, m.id DESC LIMIT ?"
            .to_string(),
        params: vec![
            Value::Text(UNKNOWN_SENDER_DISPLAY.to_string()),
            Value::Text("LavenderBear".to_string()),
            Value::BigInt(1),
            Value::BigInt(DEFAULT_LIMIT),
        ],
        expected_indexes: vec![
            IndexExpectation {
                label: "product_project_links(product_id, project_id)",
                alternatives: &[
                    "sqlite_autoindex_product_project_links",
                    "product_project_links",
                ],
            },
            IndexExpectation {
                label: "agents(project_id, name)",
                alternatives: &["idx_agents_project_name"],
            },
            IndexExpectation {
                label: "message_recipients(agent_id)",
                alternatives: &[
                    "idx_message_recipients_agent",
                    "idx_message_recipients_agent_message",
                    "idx_mr_agent_ack",
                ],
            },
        ],
        scan_sensitive_sources: &[
            "ppl",
            "product_project_links",
            "recipient",
            "agents",
            "r",
            "message_recipients",
            "m",
            "messages",
        ],
    }
}

fn active_reservations_spec() -> HotQuerySpec {
    let active_predicate = crate::queries::active_reservation_predicate_for("fr");
    HotQuerySpec {
        path: HotQueryPath::ActiveReservations,
        sql: format!(
            "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                    fr.created_ts, fr.expires_ts, COALESCE(rr.released_ts, fr.released_ts) AS released_ts \
             FROM file_reservations fr \
             LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
             WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ?"
        ),
        params: vec![Value::BigInt(1), Value::BigInt(1_700_000_000_000_000)],
        expected_indexes: vec![IndexExpectation {
            label: "file_reservations(project_id, released_ts, expires_ts)",
            alternatives: &[
                "idx_file_reservations_project_released_expires",
                "idx_file_reservations_project_agent_released",
            ],
        }],
        scan_sensitive_sources: &["fr", "file_reservations"],
    }
}

fn search_fallback_spec(query: &SearchQuery) -> HotQuerySpec {
    let mut query = query.clone();
    if query.limit.is_none() {
        query.limit = Some(usize::try_from(DEFAULT_LIMIT).unwrap_or(20));
    }
    let plan = plan_search(&query);
    HotQuerySpec {
        path: HotQueryPath::SearchFallback,
        sql: plan.sql,
        params: plan.params.into_iter().map(plan_param_to_value).collect(),
        expected_indexes: vec![IndexExpectation {
            label: "messages(project_id, created_ts)",
            alternatives: &[
                "idx_messages_project_created",
                "idx_msg_project_importance_created",
                "idx_messages_created_ts",
            ],
        }],
        scan_sensitive_sources: &["m", "messages"],
    }
}

fn plan_param_to_value(param: PlanParam) -> Value {
    match param {
        PlanParam::Int(value) => Value::BigInt(value),
        PlanParam::Text(value) => Value::Text(value),
        PlanParam::Float(value) => Value::Double(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn block_on<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(asupersync::Cx) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cx = asupersync::Cx::for_testing();
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    }

    fn test_conn() -> DbConn {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
            .expect("apply PRAGMAs");
        block_on({
            let conn = &conn;
            move |cx| async move {
                schema::migrate_to_latest_base(&cx, conn)
                    .await
                    .into_result()
                    .expect("init schema migrations");
            }
        });
        conn
    }

    #[test]
    fn hot_query_diagnostics_cover_named_paths() {
        let conn = test_conn();
        let summary = summarize_hot_query_plans(&conn).expect("summarize hot plans");

        assert_eq!(summary.diagnostics.len(), 4);
        assert_eq!(
            summary
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "inbox",
                "product_inbox",
                "active_reservations",
                "search_fallback"
            ]
        );
        assert!(
            summary
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.plan_hash.len() == 64)
        );
        assert!(
            summary
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.steps.is_empty())
        );
    }

    #[test]
    fn scan_regression_is_reported_for_unindexed_message_body_query() {
        let conn = test_conn();
        let spec = HotQuerySpec {
            path: HotQueryPath::SearchFallback,
            sql: "SELECT m.id FROM messages m WHERE m.body_md LIKE ? ORDER BY m.created_ts DESC LIMIT ?"
                .to_string(),
            params: vec![
                Value::Text("%needle%".to_string()),
                Value::BigInt(DEFAULT_LIMIT),
            ],
            expected_indexes: Vec::new(),
            scan_sensitive_sources: &["m", "messages"],
        };

        let diagnostic = explain_query_plan(&conn, &spec).expect("explain scan query");
        assert_eq!(diagnostic.status, "warn");
        assert!(
            diagnostic
                .warnings
                .iter()
                .any(|warning| warning.contains("scan m") || warning.contains("scan messages")),
            "expected full scan warning, got {:?}",
            diagnostic.warnings
        );
    }

    #[test]
    fn scan_warning_is_not_suppressed_by_unrelated_expected_index() {
        let steps = vec![
            QueryPlanStep {
                id: 1,
                parent: 0,
                notused: 0,
                detail: "SCAN m USING INDEX idx_unexpected".to_string(),
            },
            QueryPlanStep {
                id: 2,
                parent: 0,
                notused: 0,
                detail: "SEARCH r USING INDEX idx_message_recipients_agent (agent_id=?)"
                    .to_string(),
            },
        ];
        let expected_indexes = vec![IndexExpectation {
            label: "message_recipients(agent_id)",
            alternatives: &["idx_message_recipients_agent"],
        }];

        let warnings = scan_warnings(&steps, &["m", "messages"], &expected_indexes);
        assert_eq!(warnings, vec!["scan m using index idx_unexpected"]);
    }

    #[test]
    fn scan_warning_accepts_expected_index_on_same_detail() {
        let steps = vec![QueryPlanStep {
            id: 1,
            parent: 0,
            notused: 0,
            detail: "SCAN m USING INDEX idx_messages_created_ts".to_string(),
        }];
        let expected_indexes = vec![IndexExpectation {
            label: "messages(created_ts)",
            alternatives: &["idx_messages_created_ts"],
        }];

        let warnings = scan_warnings(&steps, &["m", "messages"], &expected_indexes);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn scan_warning_does_not_match_alias_prefix() {
        let steps = vec![QueryPlanStep {
            id: 1,
            parent: 0,
            notused: 0,
            detail: "SCAN message_recipients USING INDEX idx_unexpected".to_string(),
        }];
        let expected_indexes = vec![IndexExpectation {
            label: "messages(created_ts)",
            alternatives: &["idx_messages_created_ts"],
        }];

        let warnings = scan_warnings(&steps, &["m", "messages"], &expected_indexes);
        assert!(
            warnings.is_empty(),
            "alias source 'm' should not match message_recipients: {warnings:?}"
        );
    }

    #[test]
    fn plan_hash_changes_when_plan_shape_changes() {
        let conn = test_conn();
        let indexed = explain_query_plan(&conn, &inbox_spec()).expect("indexed inbox plan");
        let scan_spec = HotQuerySpec {
            path: HotQueryPath::Inbox,
            sql: "SELECT m.id FROM messages m WHERE m.body_md LIKE ? LIMIT ?".to_string(),
            params: vec![
                Value::Text("%needle%".to_string()),
                Value::BigInt(DEFAULT_LIMIT),
            ],
            expected_indexes: Vec::new(),
            scan_sensitive_sources: &["m", "messages"],
        };
        let scanned = explain_query_plan(&conn, &scan_spec).expect("scan inbox plan");

        assert_ne!(indexed.plan_hash, scanned.plan_hash);
    }
}
