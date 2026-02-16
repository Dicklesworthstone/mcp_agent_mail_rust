//! Search cluster tools
//!
//! Tools for message search and thread summarization:
//! - `search_messages`: Full-text search over messages
//! - `summarize_thread`: Extract thread summary

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};

use crate::llm;
use crate::tool_util::{db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_project};

const MAX_SUMMARIZE_THREAD_IDS: usize = 128;

/// Search result entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: i64,
    pub subject: String,
    pub importance: String,
    pub ack_required: i32,
    pub created_ts: Option<String>,
    pub thread_id: Option<String>,
    pub from: String,
    /// Concise reason codes explaining why this result ranked here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    /// Top score factors with contributions (present when explain=true).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub score_factors: Vec<mcp_agent_mail_db::search_planner::ScoreFactorSummary>,
}

/// Search response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub result: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistance: Option<mcp_agent_mail_db::QueryAssistance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<mcp_agent_mail_db::search_planner::QueryExplain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Mention count entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MentionCount {
    pub name: String,
    pub count: i64,
}

/// Aggregate top mention can be either a plain name string or a {name, count} object.
///
/// Legacy Python can emit either shape depending on whether LLM refinement overwrote the
/// heuristic `{name,count}` objects with a string[] list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TopMention {
    Name(String),
    Count(MentionCount),
}

/// Thread summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub participants: Vec<String>,
    pub key_points: Vec<String>,
    pub action_items: Vec<String>,
    pub total_messages: i64,
    pub open_actions: i64,
    pub done_actions: i64,
    pub mentions: Vec<MentionCount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_references: Option<Vec<String>>,
}

/// Single thread summary response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleThreadResponse {
    pub thread_id: String,
    pub summary: ThreadSummary,
    pub examples: Vec<ExampleMessage>,
}

/// Multi-thread aggregate response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiThreadResponse {
    pub threads: Vec<ThreadEntry>,
    pub aggregate: AggregateSummary,
}

/// Thread entry in multi-thread response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEntry {
    pub thread_id: String,
    pub summary: ThreadSummary,
}

/// Aggregate summary across threads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateSummary {
    pub top_mentions: Vec<TopMention>,
    pub key_points: Vec<String>,
    pub action_items: Vec<String>,
}

/// Example message for summaries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExampleMessage {
    pub id: i64,
    pub from: String,
    pub subject: String,
    pub created_ts: String,
}

fn is_ordered_prefix(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    matches!(bytes[0], b'1' | b'2' | b'3' | b'4' | b'5') && bytes[1] == b'.'
}

fn parse_thread_ids(thread_id: &str) -> Vec<String> {
    thread_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[allow(clippy::too_many_lines)]
pub(crate) fn summarize_messages(
    rows: &[mcp_agent_mail_db::queries::ThreadMessageRow],
) -> ThreadSummary {
    let mut participants: HashSet<String> = HashSet::with_capacity(rows.len().min(16));
    let mut key_points: Vec<String> = Vec::with_capacity(8);
    let mut action_items: Vec<String> = Vec::with_capacity(8);
    let mut open_actions: i64 = 0;
    let mut done_actions: i64 = 0;
    let mut mentions: HashMap<String, i64> = HashMap::with_capacity(8);
    let mut code_references: HashSet<String> = HashSet::with_capacity(8);
    let keywords = ["TODO", "ACTION", "FIXME", "NEXT", "BLOCKED"];

    for row in rows {
        participants.insert(row.from.clone());

        for line in row.body_md.lines() {
            let stripped = line.trim();
            if stripped.is_empty() {
                continue;
            }

            // Mentions
            for token in stripped.split_whitespace() {
                if let Some(rest) = token.strip_prefix('@') {
                    let name =
                        rest.trim_matches(&['.', ',', ':', ';', '(', ')', '[', ']', '{', '}'][..]);
                    if !name.is_empty() {
                        *mentions.entry(name.to_string()).or_insert(0) += 1;
                    }
                }
            }

            // Code references in backticks
            let mut start = 0;
            while let Some(i) = stripped[start..].find('`') {
                let i = start + i;
                if let Some(j_rel) = stripped[i + 1..].find('`') {
                    let j = i + 1 + j_rel;
                    let snippet = stripped[i + 1..j].trim();
                    if (snippet.contains('/')
                        || snippet.contains(".py")
                        || snippet.contains(".ts")
                        || snippet.contains(".md"))
                        && (1..=120).contains(&snippet.len())
                    {
                        code_references.insert(snippet.to_string());
                    }
                    start = j + 1;
                } else {
                    break;
                }
            }

            // Bullet points and ordered lists => key points
            if stripped.starts_with('-')
                || stripped.starts_with('*')
                || stripped.starts_with('+')
                || is_ordered_prefix(stripped)
            {
                let mut normalized = stripped.to_string();
                if normalized.starts_with("- [ ]")
                    || normalized.starts_with("- [x]")
                    || normalized.starts_with("- [X]")
                {
                    if let Some((_, rest)) = normalized.split_once(']') {
                        normalized = rest.trim().to_string();
                    }
                }
                let cleaned = normalized
                    .trim_start_matches(&['-', '+', '*', ' '][..])
                    .to_string();
                if !cleaned.is_empty() {
                    key_points.push(cleaned);
                }
            }

            // Checkbox actions
            if stripped.starts_with("- [ ]")
                || stripped.starts_with("* [ ]")
                || stripped.starts_with("+ [ ]")
            {
                open_actions += 1;
                action_items.push(stripped.to_string());
                continue;
            }
            if stripped.starts_with("- [x]")
                || stripped.starts_with("- [X]")
                || stripped.starts_with("* [x]")
                || stripped.starts_with("* [X]")
                || stripped.starts_with("+ [x]")
                || stripped.starts_with("+ [X]")
            {
                done_actions += 1;
                action_items.push(stripped.to_string());
                continue;
            }

            let upper = stripped.to_ascii_uppercase();
            if keywords.iter().any(|k| upper.contains(k)) {
                action_items.push(stripped.to_string());
            }
        }
    }

    let mut mentions_sorted: Vec<(String, i64)> = mentions.into_iter().collect();
    mentions_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mentions = mentions_sorted
        .into_iter()
        .take(10)
        .map(|(name, count)| MentionCount { name, count })
        .collect::<Vec<_>>();

    let mut participants: Vec<String> = participants.into_iter().collect();
    participants.sort();

    let code_refs = if code_references.is_empty() {
        None
    } else {
        let mut refs: Vec<String> = code_references.into_iter().collect();
        refs.sort();
        Some(refs.into_iter().take(10).collect())
    };

    ThreadSummary {
        participants,
        key_points: key_points.into_iter().take(10).collect(),
        action_items: action_items.into_iter().take(10).collect(),
        total_messages: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        open_actions,
        done_actions,
        mentions,
        code_references: code_refs,
    }
}

/// Parse a search mode string, returning a helpful error for invalid values.
pub(crate) fn parse_search_mode(
    mode_str: &str,
) -> Result<mcp_agent_mail_db::search_planner::RankingMode, McpError> {
    match mode_str.to_ascii_lowercase().as_str() {
        "relevance" | "bm25" => Ok(mcp_agent_mail_db::search_planner::RankingMode::Relevance),
        "recency" | "recent" => Ok(mcp_agent_mail_db::search_planner::RankingMode::Recency),
        _ => Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid ranking value: \"{mode_str}\". Valid values: \"relevance\", \"recency\"."
            ),
            true,
            json!({
                "field": "ranking",
                "provided": mode_str,
                "valid_values": ["relevance", "recency"]
            }),
        )),
    }
}

/// Parse importance filter values (comma-separated).
pub(crate) fn parse_importance_list(
    raw: &str,
) -> Result<Vec<mcp_agent_mail_db::search_planner::Importance>, McpError> {
    let mut result = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match mcp_agent_mail_db::search_planner::Importance::parse(token) {
            Some(imp) => result.push(imp),
            None => {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Invalid importance value: \"{token}\". Valid values: \"low\", \"normal\", \"high\", \"urgent\"."
                    ),
                    true,
                    json!({
                        "field": "importance",
                        "provided": token,
                        "valid_values": ["low", "normal", "high", "urgent"]
                    }),
                ));
            }
        }
    }
    Ok(result)
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeBoundary {
    StartInclusive,
    EndInclusive,
}

fn non_empty_trimmed(value: Option<String>) -> Option<String> {
    value.and_then(|candidate| {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn alias_conflict_error(
    canonical_field: &str,
    first_name: &str,
    first_value: &str,
    second_name: &str,
    second_value: &str,
) -> McpError {
    legacy_tool_error(
        "INVALID_ARGUMENT",
        format!(
            "Conflicting {canonical_field} values: {first_name}=\"{first_value}\" and {second_name}=\"{second_value}\"."
        ),
        true,
        json!({
            "field": canonical_field,
            "first": { "name": first_name, "value": first_value },
            "second": { "name": second_name, "value": second_value }
        }),
    )
}

pub(crate) fn resolve_text_filter_alias(
    canonical_field: &'static str,
    canonical_value: Option<String>,
    alias_values: &[(&'static str, Option<String>)],
) -> Result<Option<String>, McpError> {
    let mut chosen: Option<(&'static str, String)> =
        non_empty_trimmed(canonical_value).map(|value| (canonical_field, value));

    for (alias_name, alias_value) in alias_values {
        let Some(alias_value) = non_empty_trimmed(alias_value.clone()) else {
            continue;
        };
        if let Some((chosen_name, chosen_value)) = &chosen {
            if !chosen_value.eq_ignore_ascii_case(&alias_value) {
                return Err(alias_conflict_error(
                    canonical_field,
                    chosen_name,
                    chosen_value,
                    alias_name,
                    &alias_value,
                ));
            }
            continue;
        }
        chosen = Some((*alias_name, alias_value));
    }

    Ok(chosen.map(|(_, value)| value))
}

fn resolve_time_bound_alias(
    canonical_field: &'static str,
    canonical_value: Option<String>,
    alias_values: &[(&'static str, Option<String>)],
    boundary: TimeBoundary,
) -> Result<Option<i64>, McpError> {
    let mut chosen: Option<(&'static str, String, i64)> = None;

    let mut candidates = Vec::with_capacity(alias_values.len() + 1);
    candidates.push((canonical_field, canonical_value));
    candidates.extend(
        alias_values
            .iter()
            .map(|(name, value)| (*name, value.clone())),
    );

    for (name, raw_value) in candidates {
        let Some(value) = non_empty_trimmed(raw_value) else {
            continue;
        };
        let micros = parse_iso_to_micros_with_boundary(&value, name, boundary)?;
        if let Some((chosen_name, chosen_value, chosen_micros)) = &chosen {
            if *chosen_micros != micros {
                return Err(alias_conflict_error(
                    canonical_field,
                    chosen_name,
                    chosen_value,
                    name,
                    &value,
                ));
            }
            continue;
        }
        chosen = Some((name, value, micros));
    }

    Ok(chosen.map(|(_, _, micros)| micros))
}

pub(crate) fn parse_time_range_with_aliases(
    date_start: Option<String>,
    date_end: Option<String>,
    start_aliases: &[(&'static str, Option<String>)],
    end_aliases: &[(&'static str, Option<String>)],
) -> Result<mcp_agent_mail_db::search_planner::TimeRange, McpError> {
    let min_ts = resolve_time_bound_alias(
        "date_start",
        date_start,
        start_aliases,
        TimeBoundary::StartInclusive,
    )?;
    let max_ts = resolve_time_bound_alias(
        "date_end",
        date_end,
        end_aliases,
        TimeBoundary::EndInclusive,
    )?;

    if let (Some(start), Some(end)) = (min_ts, max_ts) {
        if start > end {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                "Invalid date range: date_start must be less than or equal to date_end.",
                true,
                json!({
                    "field": "date_range",
                    "date_start_micros": start,
                    "date_end_micros": end
                }),
            ));
        }
    }

    Ok(mcp_agent_mail_db::search_planner::TimeRange { min_ts, max_ts })
}

/// Full-text search over message subjects and bodies.
///
/// Supports FTS5 syntax:
/// - Phrases: "build plan"
/// - Prefix: migrat*
/// - Boolean: plan AND users
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `query`: FTS5 query string
/// - `limit`: Max results (default: 20)
/// - `offset`: Pagination offset (default: 0)
/// - `ranking`: Ranking mode: "relevance" (default) or "recency"
/// - `sender`: Filter by sender agent name (`from_agent` and `sender_name` are aliases)
/// - `importance`: Filter by importance: "low", "normal", "high", "urgent" (comma-separated)
/// - `thread_id`: Filter by thread ID
/// - `date_start`: Filter messages created at/after this ISO-8601 timestamp
/// - `date_end`: Filter messages created at/before this ISO-8601 timestamp
/// - `date_from`, `after`, `since`: Aliases of `date_start`
/// - `date_to`, `before`, `until`: Aliases of `date_end`
/// - `explain`: Include query plan explain metadata (default: false)
///
/// # Returns
/// List of matching message summaries
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
#[tool(
    description = "Full-text search over subject and body for a project.\n\nTips\n----\n- SQLite FTS5 syntax supported: phrases (\"build plan\"), prefix (mig*), boolean (plan AND users)\n- Results are ordered by bm25 score (best matches first) unless ranking=\"recency\"\n- Limit defaults to 20; raise for broad queries\n- All filter parameters are optional; omit to search without filtering\n\nQuery examples\n---------------\n- Phrase search: `\"build plan\"`\n- Prefix: `migrat*`\n- Boolean: `plan AND users`\n- Require urgent: `urgent AND deployment`\n\nParameters\n----------\nproject_key : str\n    Project identifier.\nquery : str\n    FTS5 query string.\nlimit : int\n    Max results to return (default 20, max 1000).\noffset : int\n    Pagination offset (default 0).\nranking : str\n    Ranking mode: \"relevance\" (BM25, default) or \"recency\" (newest first).\nsender : str\n    Filter by sender agent name (exact match). Aliases: `from_agent`, `sender_name`.\nimportance : str\n    Filter by importance level(s). Comma-separated: \"low\", \"normal\", \"high\", \"urgent\".\nthread_id : str\n    Filter by thread ID (exact match).\ndate_start : str\n    Inclusive lower bound for created timestamp.\ndate_end : str\n    Inclusive upper bound for created timestamp.\n    Aliases for start: `date_from`, `after`, `since`.\n    Aliases for end: `date_to`, `before`, `until`.\n    Date-only values are normalized in UTC (`date_end` includes the full day).\nexplain : bool\n    If true, include query explain metadata in the response (default false).\n\nReturns\n-------\ndict\n    { result: [{ id, subject, importance, ack_required, created_ts, thread_id, from }], assistance?, explain?, next_cursor? }\n\nExamples\n--------\nBasic search:\n```json\n{\"project_key\":\"/abs/path/backend\",\"query\":\"build plan\",\"limit\":50}\n```\n\nFiltered search:\n```json\n{\"project_key\":\"/abs/path/backend\",\"query\":\"migration\",\"sender\":\"BlueLake\",\"importance\":\"high,urgent\",\"ranking\":\"recency\"}\n```"
)]
pub async fn search_messages(
    ctx: &McpContext,
    project_key: String,
    query: String,
    limit: Option<i32>,
    offset: Option<i32>,
    ranking: Option<String>,
    sender: Option<String>,
    from_agent: Option<String>,
    sender_name: Option<String>,
    importance: Option<String>,
    thread_id: Option<String>,
    date_start: Option<String>,
    date_end: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    after: Option<String>,
    before: Option<String>,
    since: Option<String>,
    until: Option<String>,
    explain: Option<bool>,
) -> McpResult<String> {
    let max_results_raw = limit.unwrap_or(20).clamp(1, 1000);
    let max_results = max_results_raw.unsigned_abs() as usize;
    let offset_val = offset.unwrap_or(0).max(0).unsigned_abs() as usize;
    let want_explain = explain.unwrap_or(false);

    // Legacy parity: empty query returns an empty result set (no DB call).
    let trimmed = query.trim();
    if trimmed.is_empty() {
        let response = SearchResponse {
            result: Vec::new(),
            assistance: None,
            explain: None,
            next_cursor: None,
        };
        return serde_json::to_string(&response)
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")));
    }

    // Parse optional ranking mode
    let ranking_mode = match &ranking {
        Some(r) => parse_search_mode(r)?,
        None => mcp_agent_mail_db::search_planner::RankingMode::default(),
    };

    // Parse optional importance filter
    let importance_filter = match &importance {
        Some(imp) => parse_importance_list(imp)?,
        None => Vec::new(),
    };

    let sender_filter = resolve_text_filter_alias(
        "sender",
        sender,
        &[("from_agent", from_agent), ("sender_name", sender_name)],
    )?;

    // Parse optional date range timestamps (ISO-8601 → microseconds)
    let time_range = parse_time_range_with_aliases(
        date_start,
        date_end,
        &[("date_from", date_from), ("after", after), ("since", since)],
        &[("date_to", date_to), ("before", before), ("until", until)],
    )?;

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Build a SearchQuery with all facets
    let search_query = mcp_agent_mail_db::search_planner::SearchQuery {
        text: trimmed.to_string(),
        doc_kind: mcp_agent_mail_db::search_planner::DocKind::Message,
        project_id: Some(project_id),
        product_id: None,
        importance: importance_filter,
        direction: None,
        agent_name: sender_filter,
        thread_id,
        ack_required: None,
        time_range,
        ranking: ranking_mode,
        limit: Some(max_results),
        cursor: None,
        explain: want_explain,
        ..Default::default()
    };

    let planner_response = db_outcome_to_mcp_result(
        mcp_agent_mail_db::search_service::execute_search_simple(ctx.cx(), &pool, &search_query)
            .await,
    )?;

    // Map planner SearchResult → tool SearchResult (legacy format)
    // Apply offset manually (planner doesn't support offset for simple search)
    let results: Vec<SearchResult> = planner_response
        .results
        .into_iter()
        .skip(offset_val)
        .map(|r| SearchResult {
            id: r.id,
            subject: r.title,
            importance: r.importance.unwrap_or_default(),
            ack_required: i32::from(r.ack_required.unwrap_or(false)),
            created_ts: r.created_ts.map(micros_to_iso),
            thread_id: r.thread_id,
            from: r.from_agent.unwrap_or_default(),
            reason_codes: r.reason_codes,
            score_factors: r.score_factors,
        })
        .collect();

    tracing::debug!(
        "Searched messages in project {} for '{}' (limit: {}, offset: {}, ranking: {:?}, found: {})",
        project_key,
        trimmed,
        max_results,
        offset_val,
        ranking_mode,
        results.len()
    );

    let response = SearchResponse {
        result: results,
        assistance: planner_response.assistance,
        explain: planner_response.explain,
        next_cursor: planner_response.next_cursor,
    };
    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Parse ISO-8601 date strings into a `TimeRange` (microsecond timestamps).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_time_range(
    date_start: Option<&str>,
    date_end: Option<&str>,
) -> Result<mcp_agent_mail_db::search_planner::TimeRange, McpError> {
    parse_time_range_with_aliases(
        date_start.map(ToString::to_string),
        date_end.map(ToString::to_string),
        &[],
        &[],
    )
}

/// Parse an ISO-8601 timestamp string to microseconds since epoch.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_iso_to_micros(s: &str, field: &str) -> Result<i64, McpError> {
    parse_iso_to_micros_with_boundary(s, field, TimeBoundary::StartInclusive)
}

fn parse_iso_to_micros_with_boundary(
    s: &str,
    field: &str,
    boundary: TimeBoundary,
) -> Result<i64, McpError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!("Invalid {field} timestamp: empty value."),
            true,
            json!({
                "field": field,
                "provided": s
            }),
        ));
    }

    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Ok(ts.timestamp_micros());
    }

    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S"))
    {
        return Ok(naive.and_utc().timestamp_micros());
    }

    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let start_micros = date
            .and_hms_micro_opt(0, 0, 0, 0)
            .map(|naive| naive.and_utc().timestamp_micros())
            .ok_or_else(|| {
                legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!("Invalid {field} timestamp: \"{s}\"."),
                    true,
                    json!({
                        "field": field,
                        "provided": s
                    }),
                )
            })?;

        return match boundary {
            TimeBoundary::StartInclusive => Ok(start_micros),
            TimeBoundary::EndInclusive => {
                start_micros.checked_add(MICROS_PER_DAY - 1).ok_or_else(|| {
                    legacy_tool_error(
                        "INVALID_ARGUMENT",
                        format!("Invalid {field} timestamp: \"{s}\"."),
                        true,
                        json!({
                            "field": field,
                            "provided": s
                        }),
                    )
                })
            }
        };
    }

    Err(legacy_tool_error(
        "INVALID_ARGUMENT",
        format!(
            "Invalid {field} timestamp: \"{s}\". Use ISO-8601 format (e.g. \"2025-01-15T00:00:00Z\")."
        ),
        true,
        json!({
            "field": field,
            "provided": s,
            "expected_format": "ISO-8601 (e.g. 2025-01-15T00:00:00Z or 2025-01-15)"
        }),
    ))
}

/// Extract participants, key points, and action items for threads.
///
/// Single-thread mode (single `thread_id)`:
/// - Returns detailed summary with optional example messages
///
/// Multi-thread mode (comma-separated IDs like "TKT-1,TKT-2"):
/// - Returns aggregate digest across all threads
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `thread_id`: Single ID or comma-separated IDs
/// - `include_examples`: Include up to 3 sample messages (single-thread only)
/// - `llm_mode`: Refine summary with AI (if enabled)
/// - `llm_model`: Override model for AI refinement
/// - `per_thread_limit`: Max messages per thread (multi-thread mode)
#[allow(clippy::too_many_lines)]
#[tool(
    description = "Extract participants, key points, and action items for one or more threads.\n\nSingle-thread mode (thread_id is a single ID):\n- Returns detailed summary with optional example messages\n- Response: { thread_id, summary: {participants[], key_points[], action_items[]}, examples[] }\n\nMulti-thread mode (thread_id is comma-separated IDs like \"TKT-1,TKT-2,TKT-3\"):\n- Returns aggregate digest across all threads\n- Response: { threads: [{thread_id, summary}], aggregate: {top_mentions[], key_points[], action_items[]} }\n\nParameters\n----------\nproject_key : str\n    Project identifier.\nthread_id : str\n    Single thread ID for detailed summary, OR comma-separated IDs for aggregate digest.\ninclude_examples : bool\n    If true (single-thread mode only), include up to 3 sample messages.\nllm_mode : bool\n    If true and LLM is enabled, refine the summary with AI.\nllm_model : Optional[str]\n    Override model name for the LLM call.\nper_thread_limit : int\n    Max messages to consider per thread (multi-thread mode).\n\nExamples\n--------\nSingle thread:\n```json\n{\"thread_id\": \"TKT-123\", \"include_examples\": true}\n```\n\nMultiple threads:\n```json\n{\"thread_id\": \"TKT-1,TKT-2,TKT-3\"}\n```"
)]
pub async fn summarize_thread(
    ctx: &McpContext,
    project_key: String,
    thread_id: String,
    include_examples: Option<bool>,
    llm_mode: Option<bool>,
    llm_model: Option<String>,
    per_thread_limit: Option<i32>,
) -> McpResult<String> {
    let with_examples = include_examples.unwrap_or(false);
    let use_llm = llm_mode.unwrap_or(true);
    let msg_limit_raw = per_thread_limit.unwrap_or(50);
    if msg_limit_raw < 1 {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: per_thread_limit must be at least 1. Check that all parameters have valid values.",
            true,
            json!({"field":"per_thread_limit","error_detail":msg_limit_raw}),
        ));
    }
    let msg_limit = usize::try_from(msg_limit_raw).map_err(|_| {
        legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: per_thread_limit exceeds supported range. Check that all parameters have valid values.",
            true,
            json!({"field":"per_thread_limit","error_detail":msg_limit_raw}),
        )
    })?;

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Check if multi-thread mode (comma-separated)
    let thread_ids = parse_thread_ids(&thread_id);

    // Legacy parity: empty thread_id returns an empty multi-thread digest.
    if thread_ids.is_empty() {
        let response = MultiThreadResponse {
            threads: Vec::new(),
            aggregate: AggregateSummary {
                top_mentions: Vec::new(),
                key_points: Vec::new(),
                action_items: Vec::new(),
            },
        };
        return serde_json::to_string(&response)
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")));
    }

    if thread_ids.len() > MAX_SUMMARIZE_THREAD_IDS {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Too many thread IDs: {} provided, limit is {}. Reduce the comma-separated thread list.",
                thread_ids.len(),
                MAX_SUMMARIZE_THREAD_IDS
            ),
            true,
            json!({
                "field": "thread_id",
                "provided_count": thread_ids.len(),
                "limit": MAX_SUMMARIZE_THREAD_IDS,
            }),
        ));
    }

    if thread_ids.len() > 1 {
        // Multi-thread mode - aggregate across threads
        let mut all_mentions: HashMap<String, i64> = HashMap::with_capacity(16);
        let mut all_actions: Vec<String> = Vec::with_capacity(thread_ids.len() * 4);
        let mut all_points: Vec<String> = Vec::with_capacity(thread_ids.len() * 4);
        let mut threads: Vec<ThreadEntry> = Vec::with_capacity(thread_ids.len());

        for tid in &thread_ids {
            let messages = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::list_thread_messages(
                    ctx.cx(),
                    &pool,
                    project_id,
                    tid,
                    Some(msg_limit),
                )
                .await,
            )?;

            let summary = summarize_messages(&messages);
            for mention in &summary.mentions {
                *all_mentions.entry(mention.name.clone()).or_insert(0) += mention.count;
            }
            all_actions.extend(summary.action_items.clone());
            all_points.extend(summary.key_points.clone());

            threads.push(ThreadEntry {
                thread_id: tid.clone(),
                summary,
            });
        }

        let mut mentions_sorted: Vec<(String, i64)> = all_mentions.into_iter().collect();
        mentions_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top_mentions = mentions_sorted
            .into_iter()
            .take(10)
            .map(|(name, count)| TopMention::Count(MentionCount { name, count }))
            .collect();

        let mut aggregate = AggregateSummary {
            top_mentions,
            key_points: all_points.into_iter().take(25).collect(),
            action_items: all_actions.into_iter().take(25).collect(),
        };

        // LLM refinement for multi-thread (if enabled)
        let config = &mcp_agent_mail_core::Config::get();
        if use_llm && config.llm_enabled {
            let thread_context: Vec<(String, Vec<String>, Vec<String>)> = threads
                .iter()
                .take(llm::MAX_THREADS_FOR_CONTEXT)
                .map(|t| {
                    (
                        t.thread_id.clone(),
                        t.summary
                            .key_points
                            .iter()
                            .take(llm::MAX_KEY_POINTS_PER_THREAD)
                            .cloned()
                            .collect(),
                        t.summary
                            .action_items
                            .iter()
                            .take(llm::MAX_ACTIONS_PER_THREAD)
                            .cloned()
                            .collect(),
                    )
                })
                .collect();

            let system = llm::multi_thread_system_prompt();
            let user = llm::multi_thread_user_prompt(&thread_context);

            match llm::complete_system_user(
                system,
                &user,
                llm_model.as_deref(),
                Some(config.llm_temperature),
                Some(config.llm_max_tokens),
            )
            .await
            {
                Ok(output) => {
                    if let Some(parsed) = llm::parse_json_safely(&output.content) {
                        llm::apply_multi_thread_thread_revisions(&mut threads, &parsed);
                        aggregate = llm::merge_multi_thread_aggregate(&aggregate, &parsed);
                    } else {
                        tracing::debug!(
                            "summarize_thread.llm_skipped: could not parse LLM response"
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!("summarize_thread.llm_skipped: {e}");
                }
            }
        }

        let response = MultiThreadResponse { threads, aggregate };

        tracing::debug!(
            "Summarized {} threads in project {} (llm: {}, limit: {})",
            thread_ids.len(),
            project_key,
            use_llm,
            msg_limit
        );

        serde_json::to_string(&response)
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
    } else {
        // Single-thread mode
        let tid = &thread_ids[0];
        let messages = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_thread_messages(
                ctx.cx(),
                &pool,
                project_id,
                tid,
                Some(msg_limit),
            )
            .await,
        )?;

        let mut summary = summarize_messages(&messages);

        // LLM refinement (if enabled)
        let config = &mcp_agent_mail_core::Config::get();
        if use_llm && config.llm_enabled {
            let msg_tuples: Vec<(i64, String, String, String)> = messages
                .iter()
                .take(llm::MAX_MESSAGES_FOR_LLM)
                .map(|m| (m.id, m.from.clone(), m.subject.clone(), m.body_md.clone()))
                .collect();

            let system = llm::single_thread_system_prompt();
            let user = llm::single_thread_user_prompt(&msg_tuples);

            match llm::complete_system_user(
                system,
                &user,
                llm_model.as_deref(),
                Some(config.llm_temperature),
                Some(config.llm_max_tokens),
            )
            .await
            {
                Ok(output) => {
                    if let Some(parsed) = llm::parse_json_safely(&output.content) {
                        summary = llm::merge_single_thread_summary(&summary, &parsed);
                    } else {
                        tracing::debug!("thread_summary.llm_skipped: could not parse LLM response");
                    }
                }
                Err(e) => {
                    tracing::debug!("thread_summary.llm_skipped: {e}");
                }
            }
        }

        let examples = if with_examples {
            messages
                .iter()
                .take(3)
                .map(|m| ExampleMessage {
                    id: m.id,
                    from: m.from.clone(),
                    subject: m.subject.clone(),
                    created_ts: micros_to_iso(m.created_ts),
                })
                .collect()
        } else {
            Vec::new()
        };

        let response = SingleThreadResponse {
            thread_id: tid.clone(),
            summary,
            examples,
        };

        tracing::debug!(
            "Summarized thread {} in project {} (examples: {}, llm: {}, model: {:?}, messages: {})",
            tid,
            project_key,
            with_examples,
            use_llm,
            llm_model,
            messages.len()
        );

        serde_json::to_string(&response)
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::queries::ThreadMessageRow;

    #[test]
    fn parse_thread_ids_trims_and_drops_empty_values() {
        let parsed = parse_thread_ids("  br-1, , br-2 ,,br-3  ");
        assert_eq!(parsed, vec!["br-1", "br-2", "br-3"]);
    }

    #[test]
    fn parse_thread_ids_empty_input_returns_empty_vec() {
        let parsed = parse_thread_ids("  , ,   ");
        assert!(parsed.is_empty());
    }

    fn make_msg(from: &str, body: &str) -> ThreadMessageRow {
        ThreadMessageRow {
            id: 1,
            project_id: 1,
            sender_id: 1,
            thread_id: None,
            subject: "test".to_string(),
            body_md: body.to_string(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: 0,
            attachments: "[]".to_string(),
            from: from.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // is_ordered_prefix
    // -----------------------------------------------------------------------

    #[test]
    fn ordered_prefix_valid() {
        assert!(is_ordered_prefix("1. First item"));
        assert!(is_ordered_prefix("2. Second item"));
        assert!(is_ordered_prefix("5. Last supported"));
    }

    #[test]
    fn ordered_prefix_too_high() {
        // Only 1-5 are recognized
        assert!(!is_ordered_prefix("6. Sixth item"));
        assert!(!is_ordered_prefix("9. Ninth item"));
    }

    #[test]
    fn ordered_prefix_no_dot() {
        assert!(!is_ordered_prefix("1 no dot"));
    }

    #[test]
    fn ordered_prefix_too_short() {
        assert!(!is_ordered_prefix("1"));
        assert!(!is_ordered_prefix(""));
    }

    #[test]
    fn ordered_prefix_letter_start() {
        assert!(!is_ordered_prefix("a. letter"));
    }

    #[test]
    fn search_response_omits_assistance_when_absent() {
        let resp = SearchResponse {
            result: Vec::new(),
            assistance: None,
            explain: None,
            next_cursor: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize search response");
        assert!(!json.contains("assistance"));
        assert!(!json.contains("explain"));
        assert!(!json.contains("next_cursor"));
    }

    #[test]
    fn search_response_includes_assistance_when_present() {
        let resp = SearchResponse {
            result: Vec::new(),
            assistance: Some(mcp_agent_mail_db::QueryAssistance {
                query_text: "thread:br-123 migration".to_string(),
                applied_filter_hints: Vec::new(),
                did_you_mean: Vec::new(),
            }),
            explain: None,
            next_cursor: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize assisted search response");
        assert!(json.contains("assistance"));
        assert!(json.contains("thread:br-123"));
    }

    #[test]
    fn parse_search_mode_valid_values() {
        assert!(parse_search_mode("relevance").is_ok());
        assert!(parse_search_mode("recency").is_ok());
        assert!(parse_search_mode("bm25").is_ok());
        assert!(parse_search_mode("recent").is_ok());
        assert!(parse_search_mode("RELEVANCE").is_ok());
    }

    #[test]
    fn parse_search_mode_invalid_value() {
        let err = parse_search_mode("invalid");
        assert!(err.is_err());
    }

    #[test]
    fn parse_importance_list_valid() {
        let result = parse_importance_list("high,urgent").unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_importance_list_single() {
        let result = parse_importance_list("low").unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_importance_list_empty() {
        let result = parse_importance_list("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_importance_list_invalid() {
        let err = parse_importance_list("critical");
        assert!(err.is_err());
    }

    #[test]
    fn parse_time_range_valid_rfc3339() {
        let range = parse_time_range(Some("2025-01-15T00:00:00Z"), None).unwrap();
        assert!(range.min_ts.is_some());
        assert!(range.max_ts.is_none());
    }

    #[test]
    fn parse_time_range_both_bounds() {
        let range =
            parse_time_range(Some("2025-01-01T00:00:00Z"), Some("2025-12-31T23:59:59Z")).unwrap();
        assert!(range.min_ts.is_some());
        assert!(range.max_ts.is_some());
        assert!(range.min_ts.unwrap() < range.max_ts.unwrap());
    }

    #[test]
    fn parse_time_range_none() {
        let range = parse_time_range(None, None).unwrap();
        assert!(range.is_empty());
    }

    #[test]
    fn parse_time_range_invalid_format() {
        let err = parse_time_range(Some("not-a-date"), None);
        assert!(err.is_err());
    }

    #[test]
    fn parse_time_range_date_only_end_is_full_day_inclusive() {
        let range = parse_time_range(None, Some("2025-01-15")).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2025-01-15T23:59:59.999999Z")
            .expect("valid timestamp")
            .timestamp_micros();
        assert_eq!(range.max_ts, Some(expected));
    }

    #[test]
    fn parse_time_range_aliases_are_supported() {
        let range = parse_time_range_with_aliases(
            None,
            None,
            &[("since", Some("2025-01-15".to_string()))],
            &[("until", Some("2025-01-16".to_string()))],
        )
        .unwrap();
        assert!(range.min_ts.is_some());
        assert!(range.max_ts.is_some());
        assert!(range.min_ts.unwrap() < range.max_ts.unwrap());
    }

    #[test]
    fn parse_time_range_alias_conflict_is_rejected() {
        let err = parse_time_range_with_aliases(
            Some("2025-01-15T00:00:00Z".to_string()),
            None,
            &[("since", Some("2025-01-16T00:00:00Z".to_string()))],
            &[],
        );
        assert!(err.is_err());
    }

    #[test]
    fn resolve_text_filter_alias_allows_equivalent_casing() {
        let sender = resolve_text_filter_alias(
            "sender",
            Some("BlueLake".to_string()),
            &[("from_agent", Some("bluelake".to_string()))],
        )
        .unwrap();
        assert_eq!(sender.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn parse_iso_to_micros_rfc3339() {
        let micros = parse_iso_to_micros("2025-01-15T12:30:00Z", "test").unwrap();
        assert!(micros > 0);
    }

    #[test]
    fn parse_iso_to_micros_invalid() {
        let err = parse_iso_to_micros("garbage", "test");
        assert!(err.is_err());
    }

    // -----------------------------------------------------------------------
    // summarize_messages: participants
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_collects_participants() {
        let rows = vec![
            make_msg("Alice", "Hello"),
            make_msg("Bob", "Hi"),
            make_msg("Alice", "Again"),
        ];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.participants, vec!["Alice", "Bob"]);
    }

    #[test]
    fn summarize_empty_messages() {
        let summary = summarize_messages(&[]);
        assert!(summary.participants.is_empty());
        assert_eq!(summary.total_messages, 0);
    }

    // -----------------------------------------------------------------------
    // summarize_messages: mentions
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_extracts_mentions() {
        let rows = vec![
            make_msg("Alice", "cc @Bob please review"),
            make_msg("Charlie", "@Bob and @Dave check this"),
        ];
        let summary = summarize_messages(&rows);
        let bob = summary.mentions.iter().find(|m| m.name == "Bob");
        assert!(bob.is_some());
        assert_eq!(bob.unwrap().count, 2);
    }

    #[test]
    fn summarize_mention_trims_punctuation() {
        // trim_matches strips: . , : ; ( ) [ ] { }
        // Note: @mention must start a whitespace-delimited token
        let rows = vec![make_msg("Alice", "Hi @Bob, please @Charlie.")];
        let summary = summarize_messages(&rows);
        let names: Vec<_> = summary.mentions.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Bob"));
        assert!(names.contains(&"Charlie"));
    }

    // -----------------------------------------------------------------------
    // summarize_messages: key points from bullet lists
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_extracts_bullet_key_points() {
        let rows = vec![make_msg(
            "Alice",
            "- First point\n- Second point\n* Third point",
        )];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.key_points.len(), 3);
        assert_eq!(summary.key_points[0], "First point");
    }

    #[test]
    fn summarize_extracts_ordered_list_key_points() {
        let rows = vec![make_msg("Alice", "1. First\n2. Second")];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.key_points.len(), 2);
    }

    // -----------------------------------------------------------------------
    // summarize_messages: action items
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_detects_open_checkbox() {
        let rows = vec![make_msg("Alice", "- [ ] Write tests\n- [ ] Review code")];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.open_actions, 2);
        assert_eq!(summary.done_actions, 0);
    }

    #[test]
    fn summarize_detects_done_checkbox() {
        let rows = vec![make_msg("Alice", "- [x] Write tests\n- [X] Review code")];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.open_actions, 0);
        assert_eq!(summary.done_actions, 2);
    }

    #[test]
    fn summarize_detects_keyword_action_items() {
        let rows = vec![make_msg(
            "Alice",
            "TODO: fix the bug\nFIXME: handle edge case",
        )];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.action_items.len(), 2);
    }

    #[test]
    fn summarize_keyword_case_insensitive() {
        let rows = vec![make_msg("Alice", "todo handle this\nblocked on review")];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.action_items.len(), 2);
    }

    // -----------------------------------------------------------------------
    // summarize_messages: code references
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_extracts_code_references() {
        let rows = vec![make_msg(
            "Alice",
            "Check `src/main.rs` and `docs/README.md`",
        )];
        let summary = summarize_messages(&rows);
        let refs = summary.code_references.unwrap_or_default();
        assert!(refs.contains(&"src/main.rs".to_string()));
        assert!(refs.contains(&"docs/README.md".to_string()));
    }

    #[test]
    fn summarize_no_code_refs_without_path_indicators() {
        let rows = vec![make_msg("Alice", "Check `simple_name` and `another`")];
        let summary = summarize_messages(&rows);
        assert!(summary.code_references.is_none());
    }

    #[test]
    fn summarize_code_ref_requires_backticks() {
        let rows = vec![make_msg("Alice", "Check src/main.rs without backticks")];
        let summary = summarize_messages(&rows);
        assert!(summary.code_references.is_none());
    }

    // -----------------------------------------------------------------------
    // summarize_messages: limits
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_caps_key_points_at_10() {
        let body = (1..=15)
            .map(|i| format!("- Point {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = vec![make_msg("Alice", &body)];
        let summary = summarize_messages(&rows);
        assert_eq!(summary.key_points.len(), 10);
    }

    #[test]
    fn summarize_caps_mentions_at_10() {
        let body = (1..=15)
            .map(|i| format!("@user{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let rows = vec![make_msg("Alice", &body)];
        let summary = summarize_messages(&rows);
        assert!(summary.mentions.len() <= 10);
    }

    // -----------------------------------------------------------------------
    // SearchResponse serialization
    // -----------------------------------------------------------------------

    #[test]
    fn search_result_serializes_all_fields() {
        let result = SearchResult {
            id: 42,
            subject: "Test subject".to_string(),
            importance: "high".to_string(),
            ack_required: 1,
            created_ts: Some("2025-01-01T00:00:00Z".to_string()),
            thread_id: Some("thread-123".to_string()),
            from: "Alice".to_string(),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"subject\":\"Test subject\""));
        assert!(json.contains("\"importance\":\"high\""));
        assert!(json.contains("\"ack_required\":1"));
        assert!(json.contains("\"thread_id\":\"thread-123\""));
        assert!(json.contains("\"from\":\"Alice\""));
    }

    #[test]
    fn search_result_null_optional_fields() {
        let result = SearchResult {
            id: 1,
            subject: "Test".to_string(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: None,
            thread_id: None,
            from: "Bob".to_string(),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"created_ts\":null"));
        assert!(json.contains("\"thread_id\":null"));
        // Empty vecs should be omitted via skip_serializing_if
        assert!(!json.contains("reason_codes"));
        assert!(!json.contains("score_factors"));
    }

    #[test]
    fn search_result_explain_fields_present_when_populated() {
        use mcp_agent_mail_db::search_planner::ScoreFactorSummary;
        let result = SearchResult {
            id: 7,
            subject: "Explain test".to_string(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: None,
            thread_id: None,
            from: "Alice".to_string(),
            reason_codes: vec!["LexicalBm25".to_string(), "FusionWeightedBlend".to_string()],
            score_factors: vec![ScoreFactorSummary {
                key: "bm25".to_string(),
                contribution: 0.72,
                summary: "Strong BM25 match on query terms".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"reason_codes\":[\"LexicalBm25\""));
        assert!(json.contains("\"score_factors\":[{"));
        assert!(json.contains("\"key\":\"bm25\""));
        assert!(json.contains("\"contribution\":0.72"));
    }

    // -----------------------------------------------------------------------
    // TopMention enum serialization
    // -----------------------------------------------------------------------

    #[test]
    fn top_mention_name_variant_serializes_as_string() {
        let mention = TopMention::Name("Alice".to_string());
        let json = serde_json::to_string(&mention).expect("serialize");
        assert_eq!(json, "\"Alice\"");
    }

    #[test]
    fn top_mention_count_variant_serializes_as_object() {
        let mention = TopMention::Count(MentionCount {
            name: "Bob".to_string(),
            count: 5,
        });
        let json = serde_json::to_string(&mention).expect("serialize");
        assert!(json.contains("\"name\":\"Bob\""));
        assert!(json.contains("\"count\":5"));
    }

    #[test]
    fn top_mention_deserializes_from_string() {
        let mention: TopMention = serde_json::from_str("\"Charlie\"").expect("deserialize");
        assert!(matches!(mention, TopMention::Name(name) if name == "Charlie"));
    }

    #[test]
    fn top_mention_deserializes_from_object() {
        let json = r#"{"name":"Dave","count":3}"#;
        let mention: TopMention = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(mention, TopMention::Count(mc) if mc.name == "Dave" && mc.count == 3));
    }

    // -----------------------------------------------------------------------
    // ThreadSummary serialization
    // -----------------------------------------------------------------------

    #[test]
    fn thread_summary_omits_code_refs_when_none() {
        let summary = ThreadSummary {
            participants: vec!["Alice".to_string()],
            key_points: Vec::new(),
            action_items: Vec::new(),
            total_messages: 1,
            open_actions: 0,
            done_actions: 0,
            mentions: Vec::new(),
            code_references: None,
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        assert!(!json.contains("code_references"));
    }

    #[test]
    fn thread_summary_includes_code_refs_when_present() {
        let summary = ThreadSummary {
            participants: vec!["Alice".to_string()],
            key_points: Vec::new(),
            action_items: Vec::new(),
            total_messages: 1,
            open_actions: 0,
            done_actions: 0,
            mentions: Vec::new(),
            code_references: Some(vec!["src/main.rs".to_string()]),
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        assert!(json.contains("\"code_references\""));
        assert!(json.contains("\"src/main.rs\""));
    }

    // -----------------------------------------------------------------------
    // summarize_messages: edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_handles_whitespace_only_body() {
        let rows = vec![make_msg("Alice", "   \n\t  \n  ")];
        let summary = summarize_messages(&rows);
        assert!(summary.key_points.is_empty());
        assert!(summary.mentions.is_empty());
    }

    #[test]
    fn summarize_handles_very_long_lines() {
        let long_line = "x".repeat(10000);
        let rows = vec![make_msg("Alice", &format!("- {long_line}"))];
        let summary = summarize_messages(&rows);
        // Should extract the point but may truncate
        assert!(!summary.key_points.is_empty());
    }

    #[test]
    fn summarize_mention_at_start_of_line() {
        let rows = vec![make_msg("Alice", "@Bob please check this")];
        let summary = summarize_messages(&rows);
        assert!(summary.mentions.iter().any(|m| m.name == "Bob"));
    }

    #[test]
    fn summarize_mention_not_extracted_from_email() {
        // Email addresses should not be extracted as mentions
        let rows = vec![make_msg("Alice", "Contact alice@example.com")];
        let summary = summarize_messages(&rows);
        // "example.com" should not be in mentions
        assert!(!summary.mentions.iter().any(|m| m.name == "example.com"));
    }
}
