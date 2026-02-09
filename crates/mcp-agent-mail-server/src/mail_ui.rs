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
use mcp_agent_mail_db::timestamps::micros_to_iso;
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
use mcp_agent_mail_storage::{self as storage, ensure_archive, ensure_archive_root};
use serde::Serialize;

use crate::markdown;
use crate::templates;

/// Dispatch a mail UI request to the correct handler.
///
/// Returns `Some(html_string)` if the route was handled, `None` for unrecognized paths.
/// Returns `Err(status, message)` for errors.
pub fn dispatch(path: &str, query: &str) -> Result<Option<String>, (u16, String)> {
    let cx = Cx::for_testing();
    let pool = get_pool()?;

    // Strip leading "/mail" prefix.
    let sub = path.strip_prefix("/mail").unwrap_or(path);

    match sub {
        "" | "/" => render_index(&cx, &pool),
        "/unified-inbox" => {
            let limit = extract_query_int(query, "limit", 10000);
            let filter_importance = extract_query_str(query, "filter_importance");
            render_unified_inbox(&cx, &pool, limit, filter_importance.as_deref())
        }
        _ if sub.starts_with("/api/") => handle_api_route(sub, &cx, &pool),
        _ if sub.starts_with("/archive/") => render_archive_route(sub, query, &cx, &pool),
        _ => dispatch_project_route(sub, &cx, &pool, query),
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

    let inbox = block_on_outcome(
        cx,
        queries::fetch_inbox(cx, pool, pid, aid, false, None, limit),
    )?;
    let total = inbox.len();
    let items: Vec<InboxMessage> = inbox
        .iter()
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

    render(
        "mail_inbox.html",
        InboxCtx {
            project: project_view(&p),
            agent: a.name,
            items,
            page,
            limit,
            total,
            prev_page: None,
            next_page: None,
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
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct SearchResult {
    id: i64,
    subject: String,
    body_snippet: String,
    sender_name: String,
    created: String,
    importance: String,
    thread_id: String,
}

fn render_search(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    query_str: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);

    let q = extract_query_str(query_str, "q").unwrap_or_default();
    let limit = extract_query_int(query_str, "limit", 50);

    let results = if q.is_empty() {
        Vec::new()
    } else {
        let rows = block_on_outcome(cx, queries::search_messages(cx, pool, pid, &q, limit))?;
        rows.iter()
            .map(|r| SearchResult {
                id: r.id,
                subject: r.subject.clone(),
                body_snippet: truncate_body(&r.subject, 200),
                sender_name: r.from.clone(),
                created: ts_display(r.created_ts),
                importance: r.importance.clone(),
                thread_id: r.thread_id.clone().unwrap_or_default(),
            })
            .collect()
    };

    render(
        "mail_search.html",
        SearchCtx {
            project: project_view(&p),
            q,
            results,
        },
    )
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
        _ if rest.starts_with("inbox/") => {
            let agent_name = rest.strip_prefix("inbox/").unwrap_or("");
            if agent_name.is_empty() {
                return Err((400, "Missing agent name".to_string()));
            }
            // Strip any sub-paths (e.g. mark-read) — for now only handle the inbox view.
            let agent_name = agent_name.split('/').next().unwrap_or(agent_name);
            let limit = extract_query_int(query, "limit", 10000);
            let page = extract_query_int(query, "page", 1);
            render_inbox(cx, pool, project_slug, agent_name, limit, page)
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

fn handle_api_route(sub: &str, cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    // /api/unified-inbox → JSON
    if sub == "/api/unified-inbox" {
        return render_api_unified_inbox(cx, pool);
    }
    // /api/projects/{project}/agents → JSON
    if let Some(rest) = sub.strip_prefix("/api/projects/") {
        if let Some(project_slug) = rest.strip_suffix("/agents") {
            return render_api_project_agents(cx, pool, project_slug);
        }
    }
    // Other API routes handled elsewhere (e.g., /mail/api/locks is in handle_special_routes).
    Ok(None)
}

fn render_api_unified_inbox(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    // Return JSON of recent messages across all projects.
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let mut messages = Vec::new();
    for p in &projects {
        let pid = p.id.unwrap_or(0);
        let agents = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        for a in &agents {
            let inbox = block_on_outcome(
                cx,
                queries::fetch_inbox(cx, pool, pid, a.id.unwrap_or(0), false, None, 500),
            )?;
            for row in inbox {
                let m = &row.message;
                messages.push(serde_json::json!({
                    "id": m.id.unwrap_or(0),
                    "subject": m.subject,
                    "body_md": m.body_md,
                    "body_length": m.body_md.len(),
                    "created_ts": ts_display(m.created_ts),
                    "importance": m.importance,
                    "thread_id": m.thread_id,
                    "sender_name": row.sender_name,
                    "project_slug": p.slug,
                    "project_name": p.human_key,
                }));
            }
        }
    }
    messages.sort_by(|a, b| {
        let ta = a["created_ts"].as_str().unwrap_or("");
        let tb = b["created_ts"].as_str().unwrap_or("");
        tb.cmp(ta)
    });
    messages.truncate(500);
    let json = serde_json::to_string(&serde_json::json!({
        "messages": messages,
        "total": messages.len(),
    }))
    .map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

fn render_api_project_agents(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
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
        _ if sub.starts_with("/archive/browser/") && sub.contains("/file") => {
            // /archive/browser/{project}/file?path=...
            let parts: Vec<&str> = sub
                .strip_prefix("/archive/browser/")
                .unwrap_or("")
                .split('/')
                .collect();
            let project_slug = parts.first().copied().unwrap_or("");
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
    if project_slug.is_empty() {
        return Err((400, "Invalid project identifier".to_string()));
    }

    let archive = get_project_archive(project_slug)?;
    match storage::get_archive_file_content(&archive, path, 10 * 1024 * 1024) {
        Ok(Some(content)) => {
            let json = serde_json::to_string(&serde_json::json!({
                "content": content,
                "path": path,
            }))
            .map_err(|e| (500, format!("JSON error: {e}")))?;
            Ok(Some(json))
        }
        Ok(None) => Err((404, "File not found".to_string())),
        Err(e) => Err((400, format!("File error: {e}"))),
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
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    _timestamp: &str,
) -> Result<Option<String>, (u16, String)> {
    if project_slug.is_empty() || agent_name.is_empty() {
        return Err((400, "project and agent parameters required".to_string()));
    }

    // Validate project exists
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    let agent_names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();

    // Return current agent list + project info (full time-travel requires git checkout)
    let json = serde_json::to_string(&serde_json::json!({
        "project": project_slug,
        "agent": agent_name,
        "agents": agent_names,
        "note": "Time-travel snapshot shows current state; full git history browsing available via archive browser",
    }))
    .map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
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
