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
    // Multi-statement hot paths must explain their real lookup stages too.
    extra_queries: Vec<(String, Vec<Value>)>,
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
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut program_verified = 0_usize;
    let index_names = std::cell::OnceCell::new();
    for (sql, params) in std::iter::once((&spec.sql, &spec.params))
        .chain(spec.extra_queries.iter().map(|(sql, params)| (sql, params)))
    {
        let explain_sql = format!("EXPLAIN QUERY PLAN {sql}");
        let rows =
            conn.query_sync(&explain_sql, params)
                .map_err(|source| QueryPlanError::Explain {
                    path: spec.path.as_str(),
                    source,
                })?;
        let mut statement_steps = Vec::with_capacity(rows.len());
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
            statement_steps.push(QueryPlanStep {
                id,
                parent,
                notused,
                detail,
            });
        }
        let statement_warnings = scan_warnings(
            &statement_steps,
            spec.scan_sensitive_sources,
            &spec.expected_indexes,
        );
        if !statement_warnings.is_empty() {
            let verified = explain_program(conn, sql, params).is_some_and(|ops| {
                index_names
                    .get_or_init(|| sqlite_index_names(conn))
                    .as_deref()
                    .is_some_and(|names| {
                        program_seeks_expected_index(&ops, names, &spec.expected_indexes)
                    })
            });
            if verified {
                program_verified += 1;
            } else {
                warnings.extend(statement_warnings);
            }
        }
        steps.extend(statement_steps);
    }

    let missing_expected_indexes = missing_expected_indexes(&steps, &spec.expected_indexes);
    let status = if missing_expected_indexes.is_empty() && warnings.is_empty() {
        "ok"
    } else {
        "warn"
    }
    .to_string();
    let detail = if status == "ok" && program_verified > 0 {
        format!(
            "plan uses expected indexed access; {program_verified} statement(s) labelled SCAN \
             seek an expected index in their programs"
        )
    } else if status == "ok" {
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

/// One instruction of a statement's `EXPLAIN` program. The pinned engine
/// renders an `OpenRead` p4 as the bare table or index name.
#[derive(Debug, Clone)]
struct ProgramOp {
    opcode: String,
    cursor: i64,
    p4: String,
}

/// The statement's emitted program, or `None` when the engine cannot emit it
/// (its VDBE codegen does not support LEFT JOIN yet) or a row does not decode.
/// Without a program the label verdict stands.
fn explain_program(conn: &DbConn, sql: &str, params: &[Value]) -> Option<Vec<ProgramOp>> {
    let rows = conn.query_sync(&format!("EXPLAIN {sql}"), params).ok()?;
    rows.iter()
        .map(|row| {
            Some(ProgramOp {
                opcode: row.get_as(1).ok()?,
                cursor: row.get_as(2).ok()?,
                p4: row.get_as(5).ok()?,
            })
        })
        .collect()
}

/// Lowercased index names from the schema. A bare `OpenRead` name does not say
/// whether it opened a table or an index.
fn sqlite_index_names(conn: &DbConn) -> Option<Vec<String>> {
    let rows = conn
        .query_sync("SELECT name FROM sqlite_master WHERE type = 'index'", &[])
        .ok()?;
    rows.iter()
        .map(|row| {
            row.get_as::<String>(0)
                .ok()
                .map(|name| name.to_ascii_lowercase())
        })
        .collect()
}

/// Whether a statement's program reaches its rows through a seek on one of
/// the expected indexes and walks nothing from either end.
///
/// When the program seeks an index other than the planner's choice, the pinned
/// engine names the seek in `EXPLAIN QUERY PLAN` only under ORDER BY, an
/// aggregate, or a multi-column equality key. Any other such seek gets its
/// generic bytecode explain, which labels every opened table `SCAN`. The
/// unordered receipt continuation page (`sync::inbox` `RECIPIENT_NEXT_SQL`,
/// unordered since 08c47009) seeks `(agent_id, message_id)` with a range on the
/// second column, so its labels read like a full scan of
/// `message_recipients`. The opcodes decide: a seek positions a cursor opened
/// on a schema index (`index_names`) that an expectation names, and a walk
/// starts at `Rewind` or `Last`. On these integer-key paths the engine's only
/// walk is its fallback for a key value that is not an integer, which comes
/// after an `IsType` check, so a walk after the first `IsType` does not count.
fn program_seeks_expected_index(
    ops: &[ProgramOp],
    index_names: &[String],
    expected: &[IndexExpectation],
) -> bool {
    let fragments = expected
        .iter()
        .flat_map(|expectation| expectation.alternatives.iter())
        .map(|fragment| fragment.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let index_cursors = ops
        .iter()
        .filter(|op| op.opcode == "OpenRead")
        .filter_map(|op| {
            let name = op.p4.to_ascii_lowercase();
            (index_names.contains(&name)
                && fragments
                    .iter()
                    .any(|fragment| name.contains(fragment.as_str())))
            .then_some(op.cursor)
        })
        .collect::<Vec<_>>();
    let seeks_expected_index = ops.iter().any(|op| {
        matches!(
            op.opcode.as_str(),
            "SeekGE" | "SeekGT" | "SeekLE" | "SeekLT"
        ) && index_cursors.contains(&op.cursor)
    });
    let fallback_guard = ops.iter().position(|op| op.opcode == "IsType");
    let walks = ops.iter().enumerate().any(|(at, op)| {
        matches!(op.opcode.as_str(), "Rewind" | "Last")
            && fallback_guard.is_none_or(|guard| at < guard)
    });
    seeks_expected_index && !walks
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
    use crate::sync::inbox;

    HotQuerySpec {
        path: HotQueryPath::Inbox,
        sql: inbox::RECIPIENT_PAGE_SQL.to_string(),
        params: vec![Value::BigInt(1)],
        extra_queries: vec![
            (
                inbox::RECIPIENT_NEXT_SQL.to_string(),
                vec![1_i64.into(), 0_i64.into()],
            ),
            inbox::lookup_query("messages", inbox::HEADER_COLUMNS, &[1, 2, 3]),
            inbox::lookup_query("agents", "id, name", &[1, 2, 3]),
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
        extra_queries: Vec::new(),
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
        extra_queries: Vec::new(),
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
        extra_queries: Vec::new(),
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
        let inbox = summary
            .diagnostics
            .iter()
            .find(|item| item.path == "inbox")
            .unwrap();
        assert_eq!(inbox.status, "ok", "{inbox:?}");
        assert!(
            inbox.steps.len() >= 4,
            "every production lookup stage must be explained"
        );

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
    fn former_inbox_join_is_reported_as_a_scan_regression() {
        let conn = test_conn();
        let mut old = inbox_spec();
        old.sql = "SELECT m.id, r.kind, s.name FROM message_recipients r \
            JOIN messages m ON m.id = r.message_id \
            LEFT JOIN agents s ON s.id = m.sender_id \
            WHERE r.agent_id = ? AND m.project_id = ? \
            ORDER BY m.created_ts DESC LIMIT ?"
            .into();
        old.params = vec![1_i64.into(), 1_i64.into(), DEFAULT_LIMIT.into()];
        old.extra_queries.clear();
        let diagnostic = explain_query_plan(&conn, &old).unwrap();
        assert_eq!(diagnostic.status, "warn");
        assert_ne!(diagnostic.warnings.len(), 0);
        assert_ne!(diagnostic.missing_expected_indexes.len(), 0);
    }

    #[test]
    fn scan_regression_is_reported_for_unindexed_message_body_query() {
        let conn = test_conn();
        let spec = HotQuerySpec {
            path: HotQueryPath::SearchFallback,
            extra_queries: Vec::new(),
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

    fn op(opcode: &str, cursor: i64, p4: &str) -> ProgramOp {
        ProgramOp {
            opcode: opcode.to_string(),
            cursor,
            p4: p4.to_string(),
        }
    }

    /// The opcode rule that clears a SCAN label, with each wrong shape
    /// planted: a walk before the non-integer fallback guard, a walk of the
    /// index instead of a seek, a seek on an index that is not expected, a
    /// seek on the table, and a seek on a table whose name an expectation
    /// matches.
    #[test]
    fn program_seek_rule_requires_an_expected_index_seek_and_no_walk() {
        let expected = vec![IndexExpectation {
            label: "message_recipients(agent_id)",
            alternatives: &["idx_message_recipients_agent_message"],
        }];
        let index_names = vec![
            "idx_message_recipients_agent_message".to_string(),
            "idx_mr_ack_message".to_string(),
        ];
        let open_table = op("OpenRead", 0, "message_recipients");
        let open_index = op("OpenRead", 1, "idx_message_recipients_agent_message");
        let seek = op("SeekGE", 1, "");
        let fallback_walk = op("Rewind", 0, "");

        let bounded = [
            open_table.clone(),
            open_index.clone(),
            seek.clone(),
            op("IsType", 0, ""),
            fallback_walk.clone(),
        ];
        assert!(program_seeks_expected_index(
            &bounded,
            &index_names,
            &expected
        ));

        let unguarded_walk = [
            open_table.clone(),
            open_index.clone(),
            fallback_walk,
            seek.clone(),
        ];
        assert!(!program_seeks_expected_index(
            &unguarded_walk,
            &index_names,
            &expected
        ));

        let index_walk = [
            open_table.clone(),
            open_index,
            op("Rewind", 1, ""),
            op("Next", 1, ""),
        ];
        assert!(!program_seeks_expected_index(
            &index_walk,
            &index_names,
            &expected
        ));

        let unexpected_index = [
            open_table.clone(),
            op("OpenRead", 1, "idx_mr_ack_message"),
            seek,
        ];
        assert!(!program_seeks_expected_index(
            &unexpected_index,
            &index_names,
            &expected
        ));

        let table_seek = [open_table, op("SeekGE", 0, "")];
        assert!(!program_seeks_expected_index(
            &table_seek,
            &index_names,
            &expected
        ));

        let table_named_like_an_expectation = [
            op("OpenRead", 0, "product_project_links"),
            op("SeekGE", 0, ""),
        ];
        let by_table_name = vec![IndexExpectation {
            label: "product_project_links(product_id, project_id)",
            alternatives: &["product_project_links"],
        }];
        assert!(!program_seeks_expected_index(
            &table_named_like_an_expectation,
            &index_names,
            &by_table_name
        ));
    }

    /// Both receipt pages seek an expected index in their emitted programs.
    /// The continuation page's range seek is labelled SCAN by the pinned
    /// engine, so the program is what the diagnostic judges.
    #[test]
    fn unordered_receipt_pages_are_judged_by_their_programs() {
        let conn = test_conn();
        let spec = inbox_spec();
        let index_names = sqlite_index_names(&conn).expect("schema index names");
        for (sql, params) in [
            (&spec.sql, &spec.params),
            (&spec.extra_queries[0].0, &spec.extra_queries[0].1),
        ] {
            let ops = explain_program(&conn, sql, params).expect("explain program");
            assert!(
                program_seeks_expected_index(&ops, &index_names, &spec.expected_indexes),
                "{sql}: {ops:?}"
            );
        }
    }

    /// A statement whose program the engine cannot emit keeps its label
    /// verdict: no program, no clearance.
    #[test]
    fn statement_without_a_program_keeps_its_scan_warning() {
        let conn = test_conn();
        let spec = HotQuerySpec {
            path: HotQueryPath::SearchFallback,
            extra_queries: Vec::new(),
            sql: "SELECT m.id, s.name FROM messages m LEFT JOIN agents s ON s.id = m.sender_id \
                  WHERE m.body_md LIKE ? LIMIT ?"
                .to_string(),
            params: vec![
                Value::Text("%needle%".to_string()),
                Value::BigInt(DEFAULT_LIMIT),
            ],
            expected_indexes: vec![IndexExpectation {
                label: "agents primary key",
                alternatives: &["integer primary key", "primary key"],
            }],
            scan_sensitive_sources: &["m", "messages"],
        };

        let diagnostic = explain_query_plan(&conn, &spec).expect("explain left join");
        assert_eq!(diagnostic.status, "warn", "{diagnostic:?}");
        assert!(!diagnostic.warnings.is_empty(), "{diagnostic:?}");
    }

    #[test]
    fn plan_hash_changes_when_plan_shape_changes() {
        let conn = test_conn();
        let indexed = explain_query_plan(&conn, &inbox_spec()).expect("indexed inbox plan");
        let scan_spec = HotQuerySpec {
            path: HotQueryPath::Inbox,
            extra_queries: Vec::new(),
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
