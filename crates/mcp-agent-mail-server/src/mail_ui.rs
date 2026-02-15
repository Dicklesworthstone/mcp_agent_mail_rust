//! Mail UI HTTP route handlers.
//!
//! Implements the `/mail/*` HTML routes that display the agent mail web interface.
//! Each route loads data from the DB, renders a Jinja template, and returns HTML.

#![forbid(unsafe_code)]

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_db::models::{AgentRow, ProjectRow};
use mcp_agent_mail_db::pool::DbPool;
use mcp_agent_mail_db::timestamps::{micros_to_iso, micros_to_naive, now_micros};
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
use mcp_agent_mail_storage::{self as storage, ensure_archive, ensure_archive_root};
use serde::Serialize;

use crate::markdown;
use crate::templates;

/// Dispatch a mail UI request to the correct handler.
///
/// Returns `Some(html_or_json_string)` if the route was handled, `None` for unrecognized paths.
/// Returns `Err(status, message)` for errors.
///
/// `method` should be `"GET"` or `"POST"`.
/// `body` is the raw request body (only relevant for POST requests).
pub fn dispatch(
    path: &str,
    query: &str,
    method: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let cx = Cx::for_testing();
    let pool = get_pool()?;

    // Strip leading "/mail" prefix.
    let sub = path.strip_prefix("/mail").unwrap_or(path);

    match sub {
        // Python parity: GET /mail and /mail/unified-inbox → unified inbox.
        "" | "/" | "/unified-inbox" => {
            let limit = extract_query_int(query, "limit", 10000);
            let filter_importance = extract_query_str(query, "filter_importance");
            render_unified_inbox(&cx, &pool, limit, filter_importance.as_deref())
        }
        // Explicit projects list route (legacy Python: GET /mail/projects).
        "/projects" => render_projects_list(&cx, &pool),
        _ if sub.starts_with("/api/") => handle_api_route(sub, query, method, body, &cx, &pool),
        _ if sub.starts_with("/archive/") => render_archive_route(sub, query, &cx, &pool),
        _ => dispatch_project_route(sub, method, body, &cx, &pool, query),
    }
}

fn get_pool() -> Result<DbPool, (u16, String)> {
    let cfg = DbPoolConfig::from_env();
    get_or_create_pool(&cfg).map_err(|e| (500, format!("Database error: {e}")))
}

fn block_on_outcome<T>(
    _cx: &Cx,
    fut: impl std::future::Future<Output = asupersync::Outcome<T, mcp_agent_mail_db::DbError>>,
) -> Result<T, (u16, String)> {
    match block_on(fut) {
        asupersync::Outcome::Ok(v) => Ok(v),
        asupersync::Outcome::Err(e) => {
            let status = if matches!(e, mcp_agent_mail_db::DbError::NotFound { .. }) {
                404
            } else {
                500
            };
            Err((status, e.to_string()))
        }
        asupersync::Outcome::Cancelled(_) => Err((503, "Request cancelled".to_string())),
        asupersync::Outcome::Panicked(p) => Err((500, format!("Internal error: {}", p.message()))),
    }
}

fn is_fts_match_syntax_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("fts5: syntax error") || lower.contains("malformed match expression")
}

fn render(name: &str, ctx: impl Serialize) -> Result<Option<String>, (u16, String)> {
    templates::render_template(name, ctx)
        .map(Some)
        .map_err(|e| (500, format!("Template error: {e}")))
}

// ---------------------------------------------------------------------------
// Query-string helpers
// ---------------------------------------------------------------------------

fn extract_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key && !v.is_empty() {
                return Some(percent_decode_component(v));
            }
        }
    }
    None
}

fn extract_query_int(query: &str, key: &str, default: usize) -> usize {
    extract_query_str(query, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn is_valid_project_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn is_valid_archive_agent_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_valid_time_travel_timestamp(timestamp: &str) -> bool {
    // Python parity: must match prefix YYYY-MM-DDTHH:MM, with optional trailing
    // seconds/fraction/timezone.
    let bytes = timestamp.as_bytes();
    if bytes.len() < 16 {
        return false;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' {
        return false;
    }

    bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
}

fn archive_browser_file_project_slug(sub: &str) -> Option<&str> {
    let rest = sub.strip_prefix("/archive/browser/")?;
    let (project_slug, tail) = rest.split_once('/')?;
    if project_slug.is_empty() || tail != "file" {
        return None;
    }
    Some(project_slug)
}

/// Percent-decode a single URL query component.
///
/// This is intentionally minimal (no `;` separators, no nested decoding), but:
/// - preserves invalid/truncated `%` escapes verbatim
/// - decodes bytes and then interprets them as UTF-8 (lossy), so non-ASCII works
fn percent_decode_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes[i + 1];
                let lo = bytes[i + 2];
                let hex = [hi, lo];
                if let Ok(hex_str) = std::str::from_utf8(&hex) {
                    if let Ok(value) = u8::from_str_radix(hex_str, 16) {
                        out.push(value);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod query_decode_tests {
    use super::percent_decode_component;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode_component("hello"), "hello");
        assert_eq!(percent_decode_component("hello+world"), "hello world");
        assert_eq!(percent_decode_component("hello%20world"), "hello world");
        assert_eq!(percent_decode_component("%40user"), "@user");
        assert_eq!(percent_decode_component("key%3Dvalue"), "key=value");
    }

    #[test]
    fn percent_decode_invalid_hex_is_preserved() {
        assert_eq!(percent_decode_component("%ZZ"), "%ZZ");
        assert_eq!(percent_decode_component("abc%2"), "abc%2");
    }

    #[test]
    fn percent_decode_utf8_multibyte() {
        // "€" U+20AC is UTF-8 bytes E2 82 AC.
        assert_eq!(percent_decode_component("%E2%82%AC"), "€");
    }
}

#[cfg(test)]
mod utility_tests {
    use super::*;

    // --- extract_query_str ---

    #[test]
    fn extract_query_str_found() {
        assert_eq!(
            extract_query_str("page=2&q=hello", "q"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn extract_query_str_not_found() {
        assert_eq!(extract_query_str("page=2&q=hello", "missing"), None);
    }

    #[test]
    fn extract_query_str_empty_value_returns_none() {
        assert_eq!(extract_query_str("q=", "q"), None);
    }

    #[test]
    fn extract_query_str_with_encoding() {
        assert_eq!(
            extract_query_str("q=hello+world", "q"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_query_str_first_match() {
        assert_eq!(
            extract_query_str("q=first&q=second", "q"),
            Some("first".to_string())
        );
    }

    #[test]
    fn extract_query_str_empty_query() {
        assert_eq!(extract_query_str("", "q"), None);
    }

    // --- extract_query_int ---

    #[test]
    fn extract_query_int_found() {
        assert_eq!(extract_query_int("page=5&limit=20", "limit", 10), 20);
    }

    #[test]
    fn extract_query_int_not_found_returns_default() {
        assert_eq!(extract_query_int("page=5", "limit", 10), 10);
    }

    #[test]
    fn extract_query_int_invalid_number_returns_default() {
        assert_eq!(extract_query_int("limit=abc", "limit", 10), 10);
    }

    // --- truncate_body ---

    #[test]
    fn truncate_body_short_unchanged() {
        assert_eq!(truncate_body("hello", 100), "hello");
    }

    #[test]
    fn truncate_body_long_truncated() {
        let result = truncate_body("hello world this is a long body", 10);
        assert!(result.ends_with('…'));
        assert!(result.len() <= 14); // 10 bytes + ellipsis char
    }

    #[test]
    fn truncate_body_at_char_boundary() {
        // "café" is 5 bytes (é is 2 bytes), max=4 should not split the é
        let result = truncate_body("café latte", 4);
        assert!(result.ends_with('…'));
        assert!(!result.contains('é')); // Should truncate before the multibyte char
    }

    #[test]
    fn truncate_body_exact_length() {
        assert_eq!(truncate_body("hello", 5), "hello");
    }

    // --- ts_display / ts_display_opt ---

    #[test]
    fn ts_display_formats_micros() {
        let result = ts_display(1_700_000_000_000_000); // ~2023-11-14
        assert!(result.contains("2023"));
    }

    #[test]
    fn ts_display_opt_none_returns_empty() {
        assert_eq!(ts_display_opt(None), "");
    }

    #[test]
    fn ts_display_opt_some_returns_formatted() {
        let result = ts_display_opt(Some(1_700_000_000_000_000));
        assert!(!result.is_empty());
    }

    // --- archive time-travel validation ---

    #[test]
    fn project_slug_validation_python_parity() {
        assert!(is_valid_project_slug("alpha"));
        assert!(is_valid_project_slug("alpha-beta_01"));
        assert!(!is_valid_project_slug("alpha-beta_01.v2"));
        assert!(!is_valid_project_slug(""));
        assert!(!is_valid_project_slug("../etc/passwd"));
        assert!(!is_valid_project_slug("project with spaces"));
    }

    #[test]
    fn archive_agent_name_validation_python_parity() {
        assert!(is_valid_archive_agent_name("Agent123"));
        assert!(is_valid_archive_agent_name("A"));
        assert!(!is_valid_archive_agent_name(""));
        assert!(!is_valid_archive_agent_name("agent-name"));
        assert!(!is_valid_archive_agent_name("agent name"));
        assert!(!is_valid_archive_agent_name("agent!"));
    }

    #[test]
    fn time_travel_timestamp_validation_python_parity() {
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43"));
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43:59Z"));
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43:59+05:30"));
        assert!(is_valid_time_travel_timestamp(
            "2026-02-11T05:43:59.123456-08:00"
        ));

        assert!(!is_valid_time_travel_timestamp(""));
        assert!(!is_valid_time_travel_timestamp("2026-02-11"));
        assert!(!is_valid_time_travel_timestamp("2026-02-11T05"));
        assert!(!is_valid_time_travel_timestamp("not-a-timestamp"));
    }

    #[test]
    fn archive_browser_file_project_slug_parser_enforces_exact_shape() {
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo/file"),
            Some("demo")
        );
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo/file/extra"),
            None
        );
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo"),
            None
        );
    }

    #[test]
    fn time_travel_timestamp_rejects_invalid_separators() {
        assert!(!is_valid_time_travel_timestamp("2026/02/11T05:43"));
        assert!(!is_valid_time_travel_timestamp("2026-02-11 05:43"));
    }
}

// ---------------------------------------------------------------------------
// Timestamp formatting for templates
// ---------------------------------------------------------------------------

fn ts_display(micros: i64) -> String {
    micros_to_iso(micros)
}

fn ts_display_opt(micros: Option<i64>) -> String {
    micros.map_or_else(String::new, ts_display)
}

/// Format micros as "Month DD, YYYY at I:MM PM" (Python parity: `created_full`).
fn ts_display_full(micros: i64) -> String {
    micros_to_naive(micros)
        .format("%B %d, %Y at %l:%M %p")
        .to_string()
}

/// Format micros as relative time (e.g. "5m ago", "2h ago", "3d ago").
/// Python parity: `created_relative`.
fn ts_display_relative(micros: i64) -> String {
    let now = now_micros();
    let delta_secs = (now - micros) / 1_000_000;
    if delta_secs < 60 {
        "Just now".to_string()
    } else if delta_secs < 3600 {
        format!("{}m ago", delta_secs / 60)
    } else if delta_secs < 86400 {
        format!("{}h ago", delta_secs / 3600)
    } else {
        format!("{}d ago", delta_secs / 86400)
    }
}

/// Extract a plain-text excerpt from markdown body (first 150 chars).
fn body_excerpt(body: &str, max_len: usize) -> String {
    // Strip basic markdown/HTML for a clean excerpt.
    let plain: String = body
        .chars()
        .filter(|c| *c != '#' && *c != '*' && *c != '`')
        .collect();
    let trimmed = plain.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

// ---------------------------------------------------------------------------
// Route: GET /mail — project index
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IndexCtx {
    projects: Vec<IndexProject>,
}

#[derive(Serialize)]
struct IndexProject {
    slug: String,
    human_key: String,
    created_at: String,
    agent_count: usize,
}

fn render_index(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let mut items: Vec<IndexProject> = Vec::with_capacity(projects.len());
    for p in &projects {
        let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
        items.push(IndexProject {
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
            created_at: ts_display(p.created_at),
            agent_count: agents.len(),
        });
    }
    render("mail_index.html", IndexCtx { projects: items })
}

// ---------------------------------------------------------------------------
// Route: GET /mail/projects — explicit projects list (Python parity)
// ---------------------------------------------------------------------------

fn render_projects_list(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    // Reuse the index renderer — Python's /mail/projects renders the same template
    // as the old /mail root (project list view).
    render_index(cx, pool)
}

// ---------------------------------------------------------------------------
// JSON response helper (for POST endpoints returning JSON)
// ---------------------------------------------------------------------------

fn json_ok(value: &serde_json::Value) -> Result<Option<String>, (u16, String)> {
    serde_json::to_string(value)
        .map(Some)
        .map_err(|e| (500, format!("JSON error: {e}")))
}

fn json_err(status: u16, detail: &str) -> Result<Option<String>, (u16, String)> {
    let body = serde_json::json!({ "error": detail });
    let s = serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"error\":\"{detail}\"}}"));
    // Return the JSON as a successful dispatch result so the caller can set the HTTP status.
    // We embed the status in the Err variant so the server can return the right code.
    Err((status, s))
}

fn json_detail_err(status: u16, detail: &str) -> Result<Option<String>, (u16, String)> {
    let body = serde_json::json!({ "detail": detail });
    let s = serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"detail\":\"{detail}\"}}"));
    Err((status, s))
}

// ---------------------------------------------------------------------------
// Route: GET /mail/unified-inbox
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct UnifiedInboxCtx {
    projects: Vec<UnifiedProject>,
    messages: Vec<UnifiedMessage>,
    total_agents: usize,
    total_messages: usize,
    filter_importance: String,
}

#[derive(Serialize)]
struct UnifiedProject {
    id: i64,
    slug: String,
    human_key: String,
    agent_count: usize,
    agents: Vec<UnifiedAgent>,
}

#[derive(Serialize)]
struct UnifiedAgent {
    id: i64,
    name: String,
    program: String,
    model: String,
    last_active: String,
}

#[derive(Serialize)]
struct UnifiedMessage {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    created: String,
    importance: String,
    thread_id: String,
    project_slug: String,
    project_name: String,
    sender: String,
    recipients: String,
}

fn render_unified_inbox(
    cx: &Cx,
    pool: &DbPool,
    limit: usize,
    filter_importance: Option<&str>,
) -> Result<Option<String>, (u16, String)> {
    let projects_rows = block_on_outcome(cx, queries::list_projects(cx, pool))?;

    let mut projects = Vec::new();
    let mut total_agents: usize = 0;
    for p in &projects_rows {
        let pid = p.id.unwrap_or(0);
        let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        if agents_rows.is_empty() {
            continue;
        }
        total_agents += agents_rows.len();
        let agents: Vec<UnifiedAgent> = agents_rows
            .iter()
            .map(|a| UnifiedAgent {
                id: a.id.unwrap_or(0),
                name: a.name.clone(),
                program: a.program.clone(),
                model: a.model.clone(),
                last_active: ts_display(a.last_active_ts),
            })
            .collect();
        projects.push(UnifiedProject {
            id: pid,
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
            agent_count: agents.len(),
            agents,
        });
    }

    // Fetch recent messages across all projects.
    // We iterate projects and collect messages, applying limit.
    let mut messages = Vec::new();
    for p in &projects_rows {
        let pid = p.id.unwrap_or(0);
        let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        for agent in &agents_rows {
            let aid = agent.id.unwrap_or(0);
            let urgent_only = filter_importance.is_some_and(|f| {
                f.eq_ignore_ascii_case("urgent") || f.eq_ignore_ascii_case("high")
            });
            let inbox = block_on_outcome(
                cx,
                queries::fetch_inbox(cx, pool, pid, aid, urgent_only, None, limit),
            )?;
            for row in inbox {
                let m = &row.message;
                messages.push(UnifiedMessage {
                    id: m.id.unwrap_or(0),
                    subject: m.subject.clone(),
                    body_md: m.body_md.clone(),
                    body_html: markdown::render_markdown_to_safe_html(&m.body_md),
                    created: ts_display(m.created_ts),
                    importance: m.importance.clone(),
                    thread_id: m.thread_id.clone().unwrap_or_default(),
                    project_slug: p.slug.clone(),
                    project_name: p.human_key.clone(),
                    sender: row.sender_name.clone(),
                    recipients: String::new(),
                });
            }
        }
        if messages.len() >= limit {
            break;
        }
    }
    messages.sort_by(|a, b| b.created.cmp(&a.created));
    messages.truncate(limit);

    let total_messages = messages.len();
    render(
        "mail_unified_inbox.html",
        UnifiedInboxCtx {
            projects,
            messages,
            total_agents,
            total_messages,
            filter_importance: filter_importance.unwrap_or("").to_string(),
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project} — project detail
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ProjectCtx {
    project: ProjectView,
    agents: Vec<AgentView>,
}

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    slug: String,
    human_key: String,
    created_at: String,
}

#[derive(Serialize)]
struct AgentView {
    id: i64,
    name: String,
    program: String,
    model: String,
    task_description: String,
    last_active: String,
}

fn project_view(p: &ProjectRow) -> ProjectView {
    ProjectView {
        id: p.id.unwrap_or(0),
        slug: p.slug.clone(),
        human_key: p.human_key.clone(),
        created_at: ts_display(p.created_at),
    }
}

fn agent_view(a: &AgentRow) -> AgentView {
    AgentView {
        id: a.id.unwrap_or(0),
        name: a.name.clone(),
        program: a.program.clone(),
        model: a.model.clone(),
        task_description: a.task_description.clone(),
        last_active: ts_display(a.last_active_ts),
    }
}

fn render_project(cx: &Cx, pool: &DbPool, slug: &str) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    render(
        "mail_project.html",
        ProjectCtx {
            project: project_view(&p),
            agents: agents.iter().map(agent_view).collect(),
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/inbox/{agent}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct InboxCtx {
    project: ProjectView,
    agent: String,
    items: Vec<InboxMessage>,
    page: usize,
    limit: usize,
    total: usize,
    prev_page: Option<usize>,
    next_page: Option<usize>,
}

#[derive(Serialize)]
struct InboxMessage {
    id: i64,
    subject: String,
    body_html: String,
    sender: String,
    importance: String,
    thread_id: String,
    created: String,
    ack_required: bool,
    acked: bool,
}

fn render_inbox(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    limit: usize,
    page: usize,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);

    // Fetch a generous amount, then paginate client-side (Python parity).
    let fetch_limit = 10_000;
    let inbox = block_on_outcome(
        cx,
        queries::fetch_inbox(cx, pool, pid, aid, false, None, fetch_limit),
    )?;
    let total = inbox.len();

    // Offset-based pagination (Python: offset = (page - 1) * limit).
    let page = page.max(1);
    let offset = (page - 1) * limit.max(1);
    let items: Vec<InboxMessage> = inbox
        .iter()
        .skip(offset)
        .take(limit)
        .map(|row| {
            let m = &row.message;
            InboxMessage {
                id: m.id.unwrap_or(0),
                subject: m.subject.clone(),
                body_html: markdown::render_markdown_to_safe_html(&m.body_md),
                sender: row.sender_name.clone(),
                importance: m.importance.clone(),
                thread_id: m.thread_id.clone().unwrap_or_default(),
                created: ts_display(m.created_ts),
                ack_required: m.ack_required_bool(),
                acked: row.ack_ts.is_some(),
            }
        })
        .collect();

    let prev_page = if page > 1 { Some(page - 1) } else { None };
    let next_page = if offset + limit < total {
        Some(page + 1)
    } else {
        None
    };

    render(
        "mail_inbox.html",
        InboxCtx {
            project: project_view(&p),
            agent: a.name,
            items,
            page,
            limit,
            total,
            prev_page,
            next_page,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/message/{mid}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MessageCtx {
    project: ProjectView,
    message: MessageView,
    sender_name: String,
    recipients: Vec<String>,
}

#[derive(Serialize)]
struct MessageView {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    importance: String,
    thread_id: String,
    created: String,
    ack_required: bool,
}

fn render_message(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    message_id: i64,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let m = block_on_outcome(cx, queries::get_message(cx, pool, message_id))?;
    let sender = block_on_outcome(cx, queries::get_agent_by_id(cx, pool, m.sender_id))?;

    let pid = p.id.unwrap_or(0);
    let recipients = block_on_outcome(
        cx,
        queries::list_message_recipient_names_for_messages(cx, pool, pid, &[message_id]),
    )?;

    render(
        "mail_message.html",
        MessageCtx {
            project: project_view(&p),
            message: MessageView {
                id: m.id.unwrap_or(0),
                subject: m.subject.clone(),
                body_md: m.body_md.clone(),
                body_html: markdown::render_markdown_to_safe_html(&m.body_md),
                importance: m.importance.clone(),
                thread_id: m.thread_id.clone().unwrap_or_default(),
                created: ts_display(m.created_ts),
                ack_required: m.ack_required_bool(),
            },
            sender_name: sender.name,
            recipients,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/thread/{thread_id}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ThreadCtx {
    project: ProjectView,
    thread_id: String,
    thread_subject: String,
    message_count: usize,
    messages: Vec<ThreadMessage>,
}

#[derive(Serialize)]
struct ThreadMessage {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    sender: String,
    created: String,
    importance: String,
}

fn render_thread(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    thread_id: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let thread_msgs = block_on_outcome(
        cx,
        queries::list_thread_messages(cx, pool, pid, thread_id, None),
    )?;

    let messages: Vec<ThreadMessage> = thread_msgs
        .iter()
        .map(|tm| ThreadMessage {
            id: tm.id,
            subject: tm.subject.clone(),
            body_md: tm.body_md.clone(),
            body_html: markdown::render_markdown_to_safe_html(&tm.body_md),
            sender: tm.from.clone(),
            created: ts_display(tm.created_ts),
            importance: tm.importance.clone(),
        })
        .collect();

    let thread_subject = messages
        .first()
        .map_or_else(|| format!("Thread {thread_id}"), |m| m.subject.clone());
    let message_count = messages.len();

    render(
        "mail_thread.html",
        ThreadCtx {
            project: project_view(&p),
            thread_id: thread_id.to_string(),
            thread_subject,
            message_count,
            messages,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/search
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SearchCtx {
    project: ProjectView,
    q: String,
    results: Vec<WebSearchResult>,
    // Facet state (round-trips through URL params)
    order: String,
    scope: String,
    boost: bool,
    importance: Vec<String>,
    agent: String,
    thread: String,
    ack: String,
    direction: String,
    from_date: String,
    to_date: String,
    // Pagination
    next_cursor: String,
    cursor: String,
    result_count: usize,
    // Agent list for facet dropdown
    agents: Vec<AgentView>,
    // Saved recipes
    recipes: Vec<RecipeView>,
    // Deep link for current search state
    deep_link: String,
}

#[derive(Serialize)]
struct WebSearchResult {
    id: i64,
    subject: String,
    snippet: String,
    #[serde(rename = "from")]
    from_name: String,
    created: String,
    created_relative: String,
    importance: String,
    thread_id: String,
    ack_required: bool,
    score: String,
}

#[derive(Serialize)]
struct RecipeView {
    id: i64,
    name: String,
    description: String,
    route: String,
    pinned: bool,
    use_count: i64,
}

/// Extract all values for a repeated query param (e.g. `imp=high&imp=urgent`).
fn extract_query_str_all(query: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key && !v.is_empty() {
                out.push(percent_decode_component(v));
            }
        }
    }
    out
}

/// Build the deep-link URL for the current search state.
fn build_search_deep_link(project_slug: &str, query_str: &str) -> String {
    if query_str.is_empty() {
        return format!("/mail/{project_slug}/search");
    }
    format!("/mail/{project_slug}/search?{query_str}")
}

/// Highlight matched terms in a snippet by wrapping them in `<mark>` tags.
///
/// Uses a simple case-insensitive substring approach. The input body is
/// first HTML-escaped, then highlight `<mark>` tags are inserted.
fn highlight_snippet(body: &str, query: &str, max_len: usize) -> String {
    // Extract search terms from the query (strip field prefixes, quotes, operators).
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| !matches!(t.to_ascii_uppercase().as_str(), "AND" | "OR" | "NOT"))
        .map(|t| {
            // Strip field prefix like "subject:" or "body:"
            let t = t.split_once(':').map_or(t, |(_, v)| v);
            // Strip quotes
            t.trim_matches('"').to_string()
        })
        .filter(|t| t.len() >= 2)
        .collect();

    if terms.is_empty() {
        return html_escape(&truncate_body(body, max_len));
    }

    // Find the best window: center on the first matching term.
    let body_lower = body.to_ascii_lowercase();
    let mut best_pos = 0usize;
    for term in &terms {
        if let Some(pos) = body_lower.find(&term.to_ascii_lowercase()) {
            best_pos = pos;
            break;
        }
    }

    // Extract window around best_pos.
    let half = max_len / 2;
    let start = best_pos.saturating_sub(half);
    let mut end = (start + max_len).min(body.len());
    // Ensure char boundary.
    while end > start && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut s_start = start;
    while s_start < end && !body.is_char_boundary(s_start) {
        s_start += 1;
    }

    let window = &body[s_start..end];
    let prefix = if s_start > 0 { "…" } else { "" };
    let suffix = if end < body.len() { "…" } else { "" };

    // HTML-escape the window first, then insert <mark> tags.
    let escaped = html_escape(&format!("{prefix}{window}{suffix}"));

    // Apply highlighting (case-insensitive replace on the escaped text).
    let mut result = escaped;
    for term in &terms {
        let escaped_term = html_escape(term);
        if escaped_term.is_empty() {
            continue;
        }
        // Case-insensitive replacement.
        let lower = result.to_ascii_lowercase();
        let pattern = escaped_term.to_ascii_lowercase();
        let mut out = String::with_capacity(result.len() + 30);
        let mut last = 0;
        for (idx, _) in lower.match_indices(&pattern) {
            out.push_str(&result[last..idx]);
            out.push_str("<mark>");
            out.push_str(&result[idx..idx + pattern.len()]);
            out.push_str("</mark>");
            last = idx + pattern.len();
        }
        out.push_str(&result[last..]);
        result = out;
    }

    result
}

/// Minimal HTML escaping for untrusted text (before inserting <mark> tags).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
fn render_search(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    query_str: &str,
) -> Result<Option<String>, (u16, String)> {
    use mcp_agent_mail_db::search_planner::{
        Direction, Importance, RankingMode, SearchQuery, SearchResponse, TimeRange,
    };

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);

    // ── Parse all query parameters ──────────────────────────────────
    let q = extract_query_str(query_str, "q").unwrap_or_default();
    let limit = extract_query_int(query_str, "limit", 50);
    let order = extract_query_str(query_str, "order").unwrap_or_else(|| "relevance".to_string());
    let scope = extract_query_str(query_str, "scope").unwrap_or_default();
    let boost = extract_query_str(query_str, "boost").is_some();
    let cursor = extract_query_str(query_str, "cursor").unwrap_or_default();

    // Facets
    let imp_strs = extract_query_str_all(query_str, "imp");
    let importance_filter: Vec<Importance> = imp_strs
        .iter()
        .filter_map(|s| Importance::parse(s))
        .collect();

    let agent_filter = extract_query_str(query_str, "agent").unwrap_or_default();
    let thread_filter = extract_query_str(query_str, "thread").unwrap_or_default();
    let ack_filter = extract_query_str(query_str, "ack").unwrap_or_else(|| "any".to_string());
    let direction_filter = extract_query_str(query_str, "direction").unwrap_or_default();
    let from_date = extract_query_str(query_str, "from_date").unwrap_or_default();
    let to_date = extract_query_str(query_str, "to_date").unwrap_or_default();

    // ── Build search query ──────────────────────────────────────────
    let has_any_filter = !q.is_empty()
        || !importance_filter.is_empty()
        || !agent_filter.is_empty()
        || !thread_filter.is_empty()
        || ack_filter != "any"
        || !direction_filter.is_empty()
        || !from_date.is_empty()
        || !to_date.is_empty();

    let (results, next_cursor_val) = if has_any_filter {
        let time_range = TimeRange {
            min_ts: parse_date_to_micros(&from_date),
            max_ts: parse_date_to_micros_end(&to_date),
        };

        let ranking = if order == "time" {
            RankingMode::Recency
        } else {
            RankingMode::Relevance
        };

        let direction = match direction_filter.as_str() {
            "inbox" => Some(Direction::Inbox),
            "outbox" => Some(Direction::Outbox),
            _ => None,
        };

        let ack_required = match ack_filter.as_str() {
            "required" => Some(true),
            "not_required" => Some(false),
            _ => None,
        };

        let search_query = SearchQuery {
            text: q.clone(),
            doc_kind: mcp_agent_mail_db::search_planner::DocKind::Message,
            project_id: Some(pid),
            product_id: None,
            importance: importance_filter,
            direction,
            agent_name: if agent_filter.is_empty() {
                None
            } else {
                Some(agent_filter.clone())
            },
            thread_id: if thread_filter.is_empty() {
                None
            } else {
                Some(thread_filter.clone())
            },
            ack_required,
            time_range,
            ranking,
            limit: Some(limit),
            cursor: if cursor.is_empty() {
                None
            } else {
                Some(cursor.clone())
            },
            explain: false,
            ..Default::default()
        };

        let resp = match block_on_outcome(
            cx,
            mcp_agent_mail_db::search_service::execute_search_simple(cx, pool, &search_query),
        ) {
            Ok(resp) => resp,
            // Python parity: malformed FTS MATCH expressions should not 500 the UI route.
            // Return deterministic empty search results while still rendering the page.
            Err((status, msg)) if status == 500 && is_fts_match_syntax_error(&msg) => {
                SearchResponse {
                    results: Vec::new(),
                    next_cursor: None,
                    explain: None,
                    assistance: None,
                    audit: Vec::new(),
                }
            }
            Err(e) => return Err(e),
        };

        let web_results: Vec<WebSearchResult> = resp
            .results
            .iter()
            .map(|r| WebSearchResult {
                id: r.id,
                subject: r.title.clone(),
                snippet: highlight_snippet(&r.body, &q, 250),
                from_name: r.from_agent.clone().unwrap_or_default(),
                created: r.created_ts.map_or_else(String::new, ts_display),
                created_relative: r.created_ts.map_or_else(String::new, ts_display_relative),
                importance: r.importance.clone().unwrap_or_default(),
                thread_id: r.thread_id.clone().unwrap_or_default(),
                ack_required: r.ack_required.unwrap_or(false),
                score: r.score.map_or_else(String::new, |s| format!("{s:.2}")),
            })
            .collect();

        let nc = resp.next_cursor.unwrap_or_default();
        (web_results, nc)
    } else {
        (Vec::new(), String::new())
    };

    // ── Load agents for facet dropdown ──────────────────────────────
    let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
    let agents: Vec<AgentView> = agents_rows.iter().map(agent_view).collect();

    // ── Load saved recipes ──────────────────────────────────────────
    let recipes = load_recipes(pool);

    let result_count = results.len();
    let deep_link = build_search_deep_link(project_slug, query_str);

    render(
        "mail_search.html",
        SearchCtx {
            project: project_view(&p),
            q,
            results,
            order,
            scope,
            boost,
            importance: imp_strs,
            agent: agent_filter,
            thread: thread_filter,
            ack: ack_filter,
            direction: direction_filter,
            from_date,
            to_date,
            next_cursor: next_cursor_val,
            cursor,
            result_count,
            agents,
            recipes,
            deep_link,
        },
    )
}

/// Load saved search recipes from the DB (best-effort, returns empty on error).
fn load_recipes(pool: &DbPool) -> Vec<RecipeView> {
    let path = pool.sqlite_path();
    if path == ":memory:" {
        return Vec::new();
    }
    let Ok(conn) = mcp_agent_mail_db::DbConn::open_file(path) else {
        return Vec::new();
    };
    let recipes = mcp_agent_mail_db::search_recipes::list_recipes(&conn).unwrap_or_default();
    recipes
        .iter()
        .map(|r| RecipeView {
            id: r.id.unwrap_or(0),
            name: r.name.clone(),
            description: r.description.clone(),
            route: r.route_string(),
            pinned: r.pinned,
            use_count: r.use_count,
        })
        .collect()
}

/// Parse "YYYY-MM-DD" to start-of-day microseconds.
fn parse_date_to_micros(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    // Parse YYYY-MM-DD
    let parts: Vec<&str> = s.splitn(3, '-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    let dt = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    let ts = dt.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros();
    Some(ts)
}

/// Parse "YYYY-MM-DD" to end-of-day microseconds (23:59:59.999999).
fn parse_date_to_micros_end(s: &str) -> Option<i64> {
    parse_date_to_micros(s).map(|ts| ts + 86_400_000_000 - 1)
}

fn truncate_body(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let mut end = max;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/file_reservations
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FileReservationsCtx {
    project: ProjectView,
    reservations: Vec<ReservationView>,
}

#[derive(Serialize)]
struct ReservationView {
    id: i64,
    agent_name: String,
    path_pattern: String,
    exclusive: bool,
    reason: String,
    created: String,
    expires: String,
    released: String,
}

fn render_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let rows = block_on_outcome(cx, queries::list_file_reservations(cx, pool, pid, false))?;

    let mut reservations = Vec::with_capacity(rows.len());
    for r in &rows {
        let agent = block_on_outcome(cx, queries::get_agent_by_id(cx, pool, r.agent_id))
            .map_or_else(|_| format!("agent#{}", r.agent_id), |a| a.name);
        reservations.push(ReservationView {
            id: r.id.unwrap_or(0),
            agent_name: agent,
            path_pattern: r.path_pattern.clone(),
            exclusive: r.exclusive != 0,
            reason: r.reason.clone(),
            created: ts_display(r.created_ts),
            expires: ts_display(r.expires_ts),
            released: ts_display_opt(r.released_ts),
        });
    }

    render(
        "mail_file_reservations.html",
        FileReservationsCtx {
            project: project_view(&p),
            reservations,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/attachments
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AttachmentsCtx {
    project: ProjectView,
}

fn render_attachments(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    render(
        "mail_attachments.html",
        AttachmentsCtx {
            project: project_view(&p),
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/overseer/compose
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OverseerComposeCtx {
    project: ProjectView,
    agents: Vec<AgentView>,
}

fn render_overseer_compose(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
    render(
        "overseer_compose.html",
        OverseerComposeCtx {
            project: project_view(&p),
            agents: agents.iter().map(agent_view).collect(),
        },
    )
}

// ---------------------------------------------------------------------------
// Project sub-route dispatch
// ---------------------------------------------------------------------------

fn dispatch_project_route(
    sub: &str,
    method: &str,
    body: &str,
    cx: &Cx,
    pool: &DbPool,
    query: &str,
) -> Result<Option<String>, (u16, String)> {
    // sub starts with "/" and has at least the project slug.
    let sub = sub.strip_prefix('/').unwrap_or(sub);
    let (project_slug, rest) = sub.split_once('/').unwrap_or((sub, ""));

    if project_slug.is_empty() {
        return Ok(None);
    }

    match rest {
        "" => render_project(cx, pool, project_slug),
        "search" => render_search(cx, pool, project_slug, query),
        "file_reservations" => render_file_reservations(cx, pool, project_slug),
        "attachments" => render_attachments(cx, pool, project_slug),
        "overseer/compose" => render_overseer_compose(cx, pool, project_slug),
        "overseer/send" if method == "POST" => handle_overseer_send(cx, pool, project_slug, body),
        _ if rest.starts_with("inbox/") => {
            let agent_rest = rest.strip_prefix("inbox/").unwrap_or("");
            if agent_rest.is_empty() {
                return Err((400, "Missing agent name".to_string()));
            }
            // Parse agent name and optional sub-action.
            let (agent_name, action) = agent_rest.split_once('/').unwrap_or((agent_rest, ""));

            if agent_name.is_empty() {
                return Err((400, "Missing agent name".to_string()));
            }

            match (method, action) {
                ("POST", "mark-read") => handle_mark_read(cx, pool, project_slug, agent_name, body),
                ("POST", "mark-all-read") => {
                    handle_mark_all_read(cx, pool, project_slug, agent_name)
                }
                ("GET", _) => {
                    let limit = extract_query_int(query, "limit", 10000);
                    let page = extract_query_int(query, "page", 1);
                    render_inbox(cx, pool, project_slug, agent_name, limit, page)
                }
                _ => Err((405, "Method Not Allowed".to_string())),
            }
        }
        _ if rest.starts_with("message/") => {
            let mid_str = rest.strip_prefix("message/").unwrap_or("");
            let mid: i64 = mid_str
                .parse()
                .map_err(|_| (400, format!("Invalid message ID: {mid_str}")))?;
            render_message(cx, pool, project_slug, mid)
        }
        _ if rest.starts_with("thread/") => {
            let thread_id = rest.strip_prefix("thread/").unwrap_or("");
            if thread_id.is_empty() {
                return Err((400, "Missing thread ID".to_string()));
            }
            render_thread(cx, pool, project_slug, thread_id)
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// API sub-routes under /mail/api/*
// ---------------------------------------------------------------------------

fn handle_api_route(
    sub: &str,
    query: &str,
    method: &str,
    body: &str,
    cx: &Cx,
    pool: &DbPool,
) -> Result<Option<String>, (u16, String)> {
    // /api/unified-inbox → JSON
    if sub == "/api/unified-inbox" {
        return render_api_unified_inbox(cx, pool, query);
    }
    // /api/projects/{project_id}/siblings/{other_id} → POST (sibling suggestion)
    if let Some(rest) = sub.strip_prefix("/api/projects/") {
        // Check for siblings route: {project_id}/siblings/{other_id}
        if let Some((project_id_str, siblings_rest)) = rest.split_once("/siblings/") {
            if method == "POST" {
                let project_id: i64 = project_id_str
                    .parse()
                    .map_err(|_| (400, "Invalid project ID".to_string()))?;
                let other_id: i64 = siblings_rest
                    .parse()
                    .map_err(|_| (400, "Invalid sibling project ID".to_string()))?;
                return handle_sibling_update(cx, pool, project_id, other_id, body);
            }
            return Err((405, "Method Not Allowed".to_string()));
        }
        // /api/projects/{project}/agents → JSON
        if let Some(project_slug) = rest.strip_suffix("/agents") {
            return render_api_project_agents(cx, pool, project_slug);
        }
    }
    // Other API routes handled elsewhere (e.g., /mail/api/locks is in handle_special_routes).
    Ok(None)
}

fn render_api_unified_inbox(
    cx: &Cx,
    pool: &DbPool,
    query: &str,
) -> Result<Option<String>, (u16, String)> {
    // Python parity: limit (default 50000, clamped to 1000), include_projects.
    let raw_limit = extract_query_int(query, "limit", 50_000);
    let limit = raw_limit.clamp(1, 1000);
    let include_projects =
        extract_query_str(query, "include_projects").is_some_and(|v| v == "true" || v == "1");

    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let mut messages = Vec::new();
    for p in &projects {
        let pid = p.id.unwrap_or(0);
        let agents = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        for a in &agents {
            let inbox = block_on_outcome(
                cx,
                queries::fetch_inbox(cx, pool, pid, a.id.unwrap_or(0), false, None, limit),
            )?;
            for row in inbox {
                let m = &row.message;
                messages.push(serde_json::json!({
                    "id": m.id.unwrap_or(0),
                    "subject": m.subject,
                    "body_md": m.body_md,
                    "body_length": m.body_md.len(),
                    "excerpt": body_excerpt(&m.body_md, 150),
                    "created_ts": ts_display(m.created_ts),
                    "created_full": ts_display_full(m.created_ts),
                    "created_relative": ts_display_relative(m.created_ts),
                    "importance": m.importance,
                    "thread_id": m.thread_id,
                    "sender": row.sender_name,
                    "project_slug": p.slug,
                    "project_name": p.human_key,
                    "read": false,
                }));
            }
        }
    }
    messages.sort_by(|a, b| {
        let ta = a["created_ts"].as_str().unwrap_or("");
        let tb = b["created_ts"].as_str().unwrap_or("");
        tb.cmp(ta)
    });
    messages.truncate(limit);

    let mut result = serde_json::json!({ "messages": messages });
    if include_projects {
        let proj_list: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id.unwrap_or(0),
                    "slug": p.slug,
                    "human_key": p.human_key,
                    "created_at": ts_display(p.created_at),
                })
            })
            .collect();
        result["projects"] = serde_json::json!(proj_list);
    }

    let json = serde_json::to_string(&result).map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

fn render_api_project_agents(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    let mut names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable(); // Python parity: ORDER BY name
    let json = serde_json::to_string(&serde_json::json!({ "agents": names }))
        .map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

// ---------------------------------------------------------------------------
// Archive routes
// ---------------------------------------------------------------------------

/// Get archive root path from Config (for git operations).
fn get_archive_root() -> Result<std::path::PathBuf, (u16, String)> {
    let config = Config::from_env();
    let (root, _) =
        ensure_archive_root(&config).map_err(|e| (500, format!("Archive error: {e}")))?;
    Ok(root)
}

/// Get a `ProjectArchive` handle for a specific project slug.
fn get_project_archive(slug: &str) -> Result<storage::ProjectArchive, (u16, String)> {
    let config = Config::from_env();
    ensure_archive(&config, slug).map_err(|e| (500, format!("Archive error: {e}")))
}

fn render_archive_route(
    sub: &str,
    query: &str,
    cx: &Cx,
    pool: &DbPool,
) -> Result<Option<String>, (u16, String)> {
    match sub {
        "/archive/guide" => render_archive_guide(cx, pool),
        "/archive/activity" => {
            let limit = extract_query_int(query, "limit", 50).min(500);
            render_archive_activity(limit)
        }
        "/archive/timeline" => {
            let project = extract_query_str(query, "project");
            render_archive_timeline(cx, pool, project.as_deref())
        }
        "/archive/browser" => {
            let project = extract_query_str(query, "project");
            let path = extract_query_str(query, "path").unwrap_or_default();
            render_archive_browser(project.as_deref(), &path)
        }
        "/archive/network" => {
            let project = extract_query_str(query, "project");
            render_archive_network(cx, pool, project.as_deref())
        }
        "/archive/time-travel" => render_archive_time_travel(cx, pool),
        "/archive/time-travel/snapshot" => {
            let project = extract_query_str(query, "project").unwrap_or_default();
            let agent = extract_query_str(query, "agent").unwrap_or_default();
            let timestamp = extract_query_str(query, "timestamp").unwrap_or_default();
            render_archive_time_travel_snapshot(cx, pool, &project, &agent, &timestamp)
        }
        _ if archive_browser_file_project_slug(sub).is_some() => {
            // /archive/browser/{project}/file?path=...
            let project_slug = archive_browser_file_project_slug(sub).unwrap_or_default();
            let path = extract_query_str(query, "path").unwrap_or_default();
            render_archive_browser_file(project_slug, &path)
        }
        _ if sub.starts_with("/archive/commit/") => {
            let sha = sub.strip_prefix("/archive/commit/").unwrap_or("");
            render_archive_commit(sha)
        }
        _ => Ok(None),
    }
}

// -- Guide --

#[derive(Serialize)]
struct ArchiveGuideCtx {
    storage_root: String,
    total_commits: String,
    project_count: usize,
    repo_size: String,
    last_commit_time: String,
    projects: Vec<ArchiveGuideProject>,
}

#[derive(Serialize)]
struct ArchiveGuideProject {
    slug: String,
    human_key: String,
}

fn render_archive_guide(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let config = Config::from_env();
    let storage_root = config.storage_root.display().to_string();

    let (total_commits, last_commit_time, repo_size) = get_archive_root().map_or_else(
        |_| ("0".to_string(), "Never".to_string(), "N/A".to_string()),
        |root| {
            // Count commits (cap at 10_000)
            let commits = storage::get_recent_commits_extended(&root, 10_000).unwrap_or_default();
            let total = if commits.len() >= 10_000 {
                "10,000+".to_string()
            } else {
                format!("{}", commits.len())
            };
            let last = commits.first().map_or_else(
                || "Never".to_string(),
                |c| c.date.get(..10).unwrap_or(&c.date).to_string(),
            );

            let size = estimate_repo_size(&root);
            (total, last, size)
        },
    );

    let db_projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let projects: Vec<ArchiveGuideProject> = db_projects
        .iter()
        .map(|p| ArchiveGuideProject {
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
        })
        .collect();
    let project_count = projects.len();

    render(
        "archive_guide.html",
        ArchiveGuideCtx {
            storage_root,
            total_commits,
            project_count,
            repo_size,
            last_commit_time,
            projects,
        },
    )
}

/// Estimate the size of a directory tree, returned as a human-readable string.
fn estimate_repo_size(path: &std::path::Path) -> String {
    // Try `du -sh` with timeout, fall back to "Unknown"
    match std::process::Command::new("du")
        .args(["-sh", &path.display().to_string()])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .split_whitespace()
                .next()
                .unwrap_or("Unknown")
                .to_string()
        }
        _ => "Unknown".to_string(),
    }
}

// -- Activity --

#[derive(Serialize)]
struct ArchiveActivityCtx {
    commits: Vec<storage::ExtendedCommitInfo>,
}

fn render_archive_activity(limit: usize) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;
    let commits = storage::get_recent_commits_extended(&root, limit).unwrap_or_default();
    render("archive_activity.html", ArchiveActivityCtx { commits })
}

// -- Commit detail --

#[derive(Serialize)]
struct ArchiveCommitCtx {
    commit: storage::CommitDetail,
}

fn render_archive_commit(sha: &str) -> Result<Option<String>, (u16, String)> {
    if sha.is_empty() {
        return render_error("Invalid commit identifier");
    }

    let root = get_archive_root()?;
    storage::get_commit_detail(&root, sha, 5 * 1024 * 1024).map_or_else(
        |_| render_error("Commit not found"),
        |detail| render("archive_commit.html", ArchiveCommitCtx { commit: detail }),
    )
}

// -- Timeline --

#[derive(Serialize)]
struct ArchiveTimelineCtx {
    commits: Vec<storage::TimelineEntry>,
    project: String,
    project_name: String,
}

fn render_archive_timeline(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;

    // Default to first project if not specified
    let (slug, project_name) = resolve_project_slug(cx, pool, project)?;

    let commits = storage::get_timeline_commits(&root, &slug, 100).unwrap_or_default();

    render(
        "archive_timeline.html",
        ArchiveTimelineCtx {
            commits,
            project: slug,
            project_name,
        },
    )
}

/// Resolve a project slug + `human_key`, defaulting to the first project.
fn resolve_project_slug(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<(String, String), (u16, String)> {
    if let Some(slug) = project {
        let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, slug))?;
        Ok((p.slug.clone(), p.human_key))
    } else {
        let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
        let first = projects
            .first()
            .ok_or_else(|| (404, "No projects found".to_string()))?;
        Ok((first.slug.clone(), first.human_key.clone()))
    }
}

// -- Browser --

#[derive(Serialize)]
struct ArchiveBrowserCtx {
    tree: Vec<storage::TreeEntry>,
    project: String,
    path: String,
}

fn render_archive_browser(
    project: Option<&str>,
    path: &str,
) -> Result<Option<String>, (u16, String)> {
    let slug = match project {
        Some(s) if !s.is_empty() => s,
        _ => return render_error("Please select a project to browse"),
    };

    let archive = get_project_archive(slug)?;
    let tree = storage::get_archive_tree(&archive, path)
        .map_err(|e| (400, format!("Browse error: {e}")))?;

    render(
        "archive_browser.html",
        ArchiveBrowserCtx {
            tree,
            project: slug.to_string(),
            path: path.to_string(),
        },
    )
}

/// JSON API: get file content from archive.
fn render_archive_browser_file(
    project_slug: &str,
    path: &str,
) -> Result<Option<String>, (u16, String)> {
    if !is_valid_project_slug(project_slug) {
        return json_detail_err(400, "Invalid project identifier");
    }

    let archive = get_project_archive(project_slug)?;
    match storage::get_archive_file_content(&archive, path, 10 * 1024 * 1024) {
        Ok(Some(content)) => {
            // Python parity: the payload is a JSON string with file content.
            let json =
                serde_json::to_string(&content).map_err(|e| (500, format!("JSON error: {e}")))?;
            Ok(Some(json))
        }
        Err(storage::StorageError::Io(err)) if err.kind() == std::io::ErrorKind::InvalidInput => {
            json_detail_err(400, "Invalid file path")
        }
        Ok(None) | Err(_) => json_detail_err(404, "File not found"),
    }
}

// -- Network graph --

#[derive(Serialize)]
struct ArchiveNetworkCtx {
    graph: storage::CommunicationGraph,
    project: String,
    project_name: String,
}

fn render_archive_network(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;
    let (slug, project_name) = resolve_project_slug(cx, pool, project)?;

    let graph = storage::get_communication_graph(&root, &slug, 200).unwrap_or_else(|_| {
        storage::CommunicationGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    });

    render(
        "archive_network.html",
        ArchiveNetworkCtx {
            graph,
            project: slug,
            project_name,
        },
    )
}

// -- Time Travel --

#[derive(Serialize)]
struct ArchiveTimeTravelCtx {
    projects: Vec<String>,
}

fn render_archive_time_travel(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    render(
        "archive_time_travel.html",
        ArchiveTimeTravelCtx { projects: slugs },
    )
}

/// JSON API: get historical inbox snapshot at a point in time.
fn render_archive_time_travel_snapshot(
    _cx: &Cx,
    _pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    timestamp: &str,
) -> Result<Option<String>, (u16, String)> {
    if !is_valid_project_slug(project_slug) {
        return json_detail_err(400, "Invalid project identifier");
    }
    if !is_valid_archive_agent_name(agent_name) {
        return json_detail_err(400, "Invalid agent name format");
    }
    if !is_valid_time_travel_timestamp(timestamp) {
        return json_detail_err(
            400,
            "Invalid timestamp format. Use ISO 8601 format (YYYY-MM-DDTHH:MM)",
        );
    }

    let archive = get_project_archive(project_slug)?;
    let snapshot =
        match storage::get_historical_inbox_snapshot(&archive, agent_name, timestamp, 200) {
            Ok(s) => s,
            Err(err) => serde_json::json!({
                "messages": [],
                "snapshot_time": serde_json::Value::Null,
                "commit_sha": serde_json::Value::Null,
                "requested_time": timestamp,
                "error": format!("Unable to retrieve historical snapshot: {err}"),
            }),
        };

    let json = serde_json::to_string(&snapshot).map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/inbox/{agent}/mark-read
// ---------------------------------------------------------------------------

fn handle_mark_read(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let message_ids: Vec<i64> = payload
        .get("message_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_i64)
                .collect::<Vec<i64>>()
        })
        .unwrap_or_default();

    if message_ids.is_empty() {
        return json_err(400, "No message IDs provided");
    }

    // Limit to prevent abuse (Python parity: max 500).
    if message_ids.len() > 500 {
        return json_err(
            400,
            &format!(
                "Too many messages selected ({}). Maximum is 500. Use 'Mark All Read' instead.",
                message_ids.len()
            ),
        );
    }

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);

    let mut marked_count = 0i64;
    for mid in &message_ids {
        match block_on_outcome(cx, queries::mark_message_read(cx, pool, aid, *mid)) {
            Ok(_) => marked_count += 1,
            Err((404, _)) => {} // Not found — already read or not a recipient; skip.
            Err(e) => return Err(e),
        }
    }

    json_ok(&serde_json::json!({
        "success": true,
        "marked_count": marked_count,
        "requested_count": message_ids.len(),
        "agent": agent_name,
        "project": p.slug,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/inbox/{agent}/mark-all-read
// ---------------------------------------------------------------------------

fn handle_mark_all_read(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);

    // Fetch all inbox messages and mark each as read.
    // `mark_message_read` is idempotent — it uses COALESCE(read_ts, now) so
    // already-read messages are harmlessly skipped.
    let inbox = block_on_outcome(
        cx,
        queries::fetch_inbox(cx, pool, pid, aid, false, None, 10_000),
    )?;

    let mut marked_count = 0i64;
    for row in &inbox {
        let mid = row.message.id.unwrap_or(0);
        if block_on_outcome(cx, queries::mark_message_read(cx, pool, aid, mid)).is_ok() {
            marked_count += 1;
        }
    }

    json_ok(&serde_json::json!({
        "success": true,
        "marked_count": marked_count,
        "agent": agent_name,
        "project": p.slug,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/overseer/send
// ---------------------------------------------------------------------------

const OVERSEER_PREAMBLE: &str = "---\n\n\
    MESSAGE FROM HUMAN OVERSEER\n\n\
    This message is from a human operator overseeing this project. \
    Please prioritize the instructions below over your current tasks.\n\n\
    You should:\n\
    1. Temporarily pause your current work\n\
    2. Complete the request described below\n\
    3. Resume your original plans afterward (unless modified by these instructions)\n\n\
    The human's guidance supersedes all other priorities.\n\n\
    ---\n\n";

#[derive(Debug)]
struct OverseerPayload {
    recipients: Vec<String>,
    subject: String,
    body_md: String,
    thread_id: Option<String>,
}

fn parse_overseer_body(body: &str) -> Result<OverseerPayload, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let recipients: Vec<String> = payload
        .get("recipients")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let subject = payload
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let body_md = payload
        .get("body_md")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let thread_id = payload
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Validation (Python parity).
    let err = |msg: &str| -> (u16, String) {
        let body = serde_json::json!({ "error": msg });
        (
            400,
            serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"error\":\"{msg}\"}}")),
        )
    };
    if recipients.is_empty() {
        return Err(err("At least one recipient is required"));
    }
    if recipients.len() > 100 {
        return Err(err("Too many recipients (maximum 100 agents)"));
    }
    if subject.is_empty() {
        return Err(err("Subject is required"));
    }
    if subject.len() > 200 {
        return Err(err("Subject too long (maximum 200 characters)"));
    }
    if body_md.is_empty() {
        return Err(err("Message body is required"));
    }
    if body_md.len() > 50_000 {
        return Err(err("Message body too long (maximum 50,000 characters)"));
    }

    // Deduplicate recipients while preserving order.
    let mut seen = std::collections::HashSet::new();
    let recipients: Vec<String> = recipients
        .into_iter()
        .filter(|r| seen.insert(r.clone()))
        .collect();

    Ok(OverseerPayload {
        recipients,
        subject,
        body_md,
        thread_id,
    })
}

fn handle_overseer_send(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let parsed = parse_overseer_body(body)?;
    let full_body = format!("{OVERSEER_PREAMBLE}{}", parsed.body_md);

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);

    // Ensure HumanOverseer agent exists.
    let overseer = block_on_outcome(
        cx,
        queries::register_agent(
            cx,
            pool,
            pid,
            "HumanOverseer",
            "WebUI",
            "Human",
            Some("Human operator providing guidance and oversight to agents"),
            Some("auto"),
        ),
    )?;
    let overseer_id = overseer.id.unwrap_or(0);

    // Resolve valid recipient agent IDs.
    let mut valid: Vec<(String, i64)> = Vec::new();
    for name in &parsed.recipients {
        if let Ok(a) = block_on_outcome(cx, queries::get_agent(cx, pool, pid, name)) {
            valid.push((name.clone(), a.id.unwrap_or(0)));
        }
    }

    if valid.is_empty() {
        return json_err(
            400,
            &format!(
                "None of the specified recipients exist in this project. \
                 Available agents can be seen at /mail/{project_slug}"
            ),
        );
    }

    // Build recipients slice for the DB call.
    let recipient_pairs: Vec<(i64, &str)> = valid.iter().map(|(_, id)| (*id, "to")).collect();

    let msg = block_on_outcome(
        cx,
        queries::create_message_with_recipients(
            cx,
            pool,
            pid,
            overseer_id,
            &parsed.subject,
            &full_body,
            parsed.thread_id.as_deref(),
            "high", // Always high importance for overseer
            false,
            "[]",
            &recipient_pairs,
        ),
    )?;

    let valid_names: Vec<&str> = valid.iter().map(|(n, _)| n.as_str()).collect();
    let created = ts_display(msg.created_ts);

    json_ok(&serde_json::json!({
        "success": true,
        "message_id": msg.id.unwrap_or(0),
        "recipients": valid_names,
        "sent_at": created,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/api/projects/{id}/siblings/{other_id}
// ---------------------------------------------------------------------------

fn handle_sibling_update(
    _cx: &Cx,
    _pool: &DbPool,
    project_id: i64,
    other_id: i64,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    if !matches!(action.as_str(), "confirm" | "dismiss" | "reset") {
        return json_err(400, "Invalid action");
    }

    let target_status = match action.as_str() {
        "confirm" => "confirmed",
        "dismiss" => "dismissed",
        "reset" => "suggested",
        _ => unreachable!(),
    };

    // Note: The sibling suggestion feature requires a `project_siblings` table
    // that is not yet part of the Rust schema. Return a stub response that acknowledges
    // the action. When the schema is added, this handler will execute the DB update.
    json_ok(&serde_json::json!({
        "status": target_status,
        "suggestion": {
            "project_id": project_id,
            "other_id": other_id,
            "status": target_status,
        },
    }))
}

/// Render an error page.
fn render_error(message: &str) -> Result<Option<String>, (u16, String)> {
    #[derive(Serialize)]
    struct ErrorCtx {
        message: String,
    }
    render(
        "error.html",
        ErrorCtx {
            message: message.to_string(),
        },
    )
}

#[cfg(test)]
mod overseer_form_validation_tests {
    use super::parse_overseer_body;
    use serde_json::json;

    fn parse_err_message(body: &str) -> (u16, String) {
        let (status, payload) = parse_overseer_body(body).expect_err("expected parse error");
        let msg = serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(payload);
        (status, msg)
    }

    #[test]
    fn parse_overseer_body_success_and_deduplicates_recipients() {
        let body = json!({
            "recipients": ["BlueLake", "GreenField", "BlueLake"],
            "subject": "  Operator notice  ",
            "body_md": "  Please prioritize this task.  ",
            "thread_id": "br-123",
        })
        .to_string();

        let parsed = parse_overseer_body(&body).expect("valid payload");
        assert_eq!(parsed.recipients, vec!["BlueLake", "GreenField"]);
        assert_eq!(parsed.subject, "Operator notice");
        assert_eq!(parsed.body_md, "Please prioritize this task.");
        assert_eq!(parsed.thread_id.as_deref(), Some("br-123"));
    }

    #[test]
    fn parse_overseer_body_rejects_invalid_json() {
        let (status, msg) = parse_overseer_body("{not-json").expect_err("invalid json should fail");
        assert_eq!(status, 400);
        assert!(msg.contains("Invalid JSON"), "unexpected message: {msg}");
    }

    #[test]
    fn parse_overseer_body_requires_recipients() {
        let body = json!({
            "recipients": [],
            "subject": "hi",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "At least one recipient is required");
    }

    #[test]
    fn parse_overseer_body_enforces_recipient_limit() {
        let recipients: Vec<String> = (0..101).map(|i| format!("Agent{i}")).collect();
        let body = json!({
            "recipients": recipients,
            "subject": "hi",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Too many recipients (maximum 100 agents)");
    }

    #[test]
    fn parse_overseer_body_requires_non_empty_subject() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "   ",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Subject is required");
    }

    #[test]
    fn parse_overseer_body_enforces_subject_length() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "x".repeat(201),
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Subject too long (maximum 200 characters)");
    }

    #[test]
    fn parse_overseer_body_requires_non_empty_body() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
            "body_md": "   ",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Message body is required");
    }

    #[test]
    fn parse_overseer_body_enforces_body_length() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
            "body_md": "x".repeat(50_001),
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Message body too long (maximum 50,000 characters)");
    }
}
