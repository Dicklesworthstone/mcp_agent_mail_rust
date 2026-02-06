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
}

/// Search response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub result: Vec<SearchResult>,
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

#[allow(clippy::too_many_lines)]
pub(crate) fn summarize_messages(
    rows: &[mcp_agent_mail_db::queries::ThreadMessageRow],
) -> ThreadSummary {
    let mut participants: HashSet<String> = HashSet::new();
    let mut key_points: Vec<String> = Vec::new();
    let mut action_items: Vec<String> = Vec::new();
    let mut open_actions: i64 = 0;
    let mut done_actions: i64 = 0;
    let mut mentions: HashMap<String, i64> = HashMap::new();
    let mut code_references: HashSet<String> = HashSet::new();
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
///
/// # Returns
/// List of matching message summaries
#[tool(description = "Full-text search over subject and body for a project.")]
pub async fn search_messages(
    ctx: &McpContext,
    project_key: String,
    query: String,
    limit: Option<i32>,
) -> McpResult<String> {
    let max_results_raw = limit.unwrap_or(20);
    if max_results_raw < 1 {
        return Err(legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit must be at least 1, got {max_results_raw}. Use a positive integer."),
            true,
            json!({"provided": max_results_raw, "min": 1, "max": 1000}),
        ));
    }
    let max_results = usize::try_from(max_results_raw).map_err(|_| {
        legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit exceeds supported range: {max_results_raw}"),
            true,
            json!({"provided": max_results_raw, "min": 1, "max": 1000}),
        )
    })?;

    // Validate query is not empty
    if query.trim().is_empty() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: Query cannot be empty. Check that all parameters have valid values.",
            true,
            json!({"field":"query","error_detail":"empty"}),
        ));
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Execute search (currently uses LIKE; will be upgraded to FTS5)
    let rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::search_messages(
            ctx.cx(),
            &pool,
            project_id,
            &query,
            max_results,
        )
        .await,
    )?;

    let results: Vec<SearchResult> = rows
        .into_iter()
        .map(|r| SearchResult {
            id: r.id,
            subject: r.subject,
            importance: r.importance,
            ack_required: i32::try_from(r.ack_required).unwrap_or(i32::MAX),
            created_ts: Some(micros_to_iso(r.created_ts)),
            thread_id: r.thread_id,
            from: r.from,
        })
        .collect();

    tracing::debug!(
        "Searched messages in project {} for '{}' (limit: {}, found: {})",
        project_key,
        query,
        max_results,
        results.len()
    );

    let response = SearchResponse { result: results };
    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
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
#[tool(description = "Extract participants, key points, and action items for threads.")]
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
    let thread_ids: Vec<&str> = thread_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if thread_ids.is_empty() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: thread_id cannot be empty. Check that all parameters have valid values.",
            true,
            json!({"field":"thread_id","error_detail":"empty"}),
        ));
    }

    if thread_ids.len() > 1 {
        // Multi-thread mode - aggregate across threads
        let mut all_mentions: HashMap<String, i64> = HashMap::new();
        let mut all_actions: Vec<String> = Vec::new();
        let mut all_points: Vec<String> = Vec::new();
        let mut threads: Vec<ThreadEntry> = Vec::new();

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
                thread_id: (*tid).to_string(),
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
        let config = mcp_agent_mail_core::Config::from_env();
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
        let tid = thread_ids[0];
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
        let config = mcp_agent_mail_core::Config::from_env();
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
            thread_id: tid.to_string(),
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
}
