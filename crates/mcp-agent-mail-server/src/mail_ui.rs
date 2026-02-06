//! Mail UI HTTP route handlers.
//!
//! Implements the `/mail/*` HTML routes that display the agent mail web interface.
//! Each route loads data from the DB, renders a Jinja template, and returns HTML.

#![forbid(unsafe_code)]

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_db::models::{AgentRow, ProjectRow};
use mcp_agent_mail_db::pool::DbPool;
use mcp_agent_mail_db::timestamps::micros_to_iso;
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
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
        _ if sub.starts_with("/archive/") => render_archive_route(sub, &cx, &pool),
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
                return Some(urlencoding_decode(v));
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

/// Minimal percent-decoding (covers the common cases for query params).
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        match b {
            b'+' => out.push(' '),
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                let val = hex_val(hi) * 16 + hex_val(lo);
                out.push(char::from(val));
            }
            _ => out.push(char::from(b)),
        }
    }
    out
}

const fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
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
    agent: AgentView,
    messages: Vec<InboxMessage>,
    page: usize,
    limit: usize,
    total: usize,
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
    let messages: Vec<InboxMessage> = inbox
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
            agent: agent_view(&a),
            messages,
            page,
            limit,
            total,
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
// Archive routes (minimal stubs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ArchiveCtx {
    projects: Vec<String>,
}

fn render_archive_route(
    sub: &str,
    cx: &Cx,
    pool: &DbPool,
) -> Result<Option<String>, (u16, String)> {
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();

    let template = match sub {
        "/archive/guide" => "archive_guide.html",
        "/archive/activity" => "archive_activity.html",
        "/archive/timeline" => "archive_timeline.html",
        "/archive/browser" => "archive_browser.html",
        "/archive/network" => "archive_network.html",
        "/archive/time-travel" => "archive_time_travel.html",
        _ if sub.starts_with("/archive/commit/") => "archive_commit.html",
        _ => return Ok(None),
    };

    render(template, ArchiveCtx { projects: slugs })
}
