//! Static HTML pre-rendering pipeline for deterministic export.
//!
//! Generates pre-rendered HTML pages, navigation structures, and search index
//! artifacts from an exported SQLite database for hosting on GitHub Pages,
//! Cloudflare Pages, or any static file server.
//!
//! The generated HTML provides:
//! - Readable no-JS fallback pages for each message, thread, and project
//! - Navigation links between all discoverable routes
//! - A machine-readable sitemap for deployment validation
//! - A search index JSON for client-side full-text search
//!
//! All output is deterministic: running the pipeline twice on the same input
//! produces byte-identical output (sorted keys, stable iteration order,
//! no embedded timestamps unless from source data).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mcp_agent_mail_db::DbConn;
use serde::{Deserialize, Serialize};
use sqlmodel_core::Value as SqlValue;

use crate::{ExportRedactionPolicy, RedactionAuditLog, RedactionReason, ShareError, ShareResult};

// ── Configuration ───────────────────────────────────────────────────────

/// Options controlling the static rendering pipeline.
#[derive(Debug, Clone)]
pub struct StaticRenderConfig {
    /// Maximum number of messages to include per project page (pagination).
    pub messages_per_page: usize,
    /// Maximum body length (characters) to include in search index entries.
    pub search_snippet_len: usize,
    /// Base path prefix for all generated links (e.g., "/viewer/pages").
    pub base_path: String,
    /// Whether to include message bodies in pre-rendered HTML.
    pub include_bodies: bool,
    /// Title prefix for HTML pages.
    pub site_title: String,
    /// Export-time redaction policy. Controls defense-in-depth secret scanning,
    /// body redaction enforcement, and recipient visibility.
    pub redaction: ExportRedactionPolicy,
}

impl Default for StaticRenderConfig {
    fn default() -> Self {
        Self {
            messages_per_page: 200,
            search_snippet_len: 300,
            base_path: ".".to_string(),
            include_bodies: true,
            site_title: "MCP Agent Mail".to_string(),
            redaction: ExportRedactionPolicy::default(),
        }
    }
}

// ── Output types ────────────────────────────────────────────────────────

/// Result of the static rendering pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticRenderResult {
    /// Total number of HTML pages generated.
    pub pages_generated: usize,
    /// Total number of projects discovered.
    pub projects_count: usize,
    /// Total number of messages rendered.
    pub messages_count: usize,
    /// Total number of threads rendered.
    pub threads_count: usize,
    /// Search index entry count.
    pub search_index_entries: usize,
    /// Paths of all generated files (relative to output dir).
    pub generated_files: Vec<String>,
    /// Audit log of all redaction actions taken during this render.
    pub redaction_audit: RedactionAuditLog,
}

/// A single entry in the sitemap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SitemapEntry {
    pub route: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(rename = "type")]
    pub entry_type: String,
}

/// A single entry in the search index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexEntry {
    pub id: i64,
    pub subject: String,
    pub snippet: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    pub importance: String,
    pub created_ts: String,
    pub route: String,
}

// ── Internal data structs ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ProjectInfo {
    slug: String,
    human_key: String,
    message_count: i64,
    agent_count: i64,
}

#[derive(Debug, Clone)]
struct MessageInfo {
    id: i64,
    subject: String,
    body_md: String,
    importance: String,
    created_ts: String,
    sender_name: String,
    project_slug: String,
    thread_id: Option<String>,
    recipients: Vec<String>,
}

#[derive(Debug, Clone)]
struct ThreadInfo {
    thread_id: String,
    project_slug: String,
    subject: String,
    message_count: usize,
    participants: BTreeSet<String>,
    latest_ts: String,
}

// ── Redaction helpers ────────────────────────────────────────────────────

const BODY_REDACTED_PLACEHOLDER: &str = "[Message body redacted]";

/// Check if a message body appears to be a redacted placeholder.
fn is_redacted_body(body: &str) -> bool {
    let trimmed = body.trim();
    trimmed == BODY_REDACTED_PLACEHOLDER || trimmed.starts_with("[Message body redacted")
}

/// Apply defense-in-depth secret scanning to a string.
/// Returns the sanitized string and whether any secrets were found.
fn defense_scan(input: &str, policy: &ExportRedactionPolicy) -> (String, bool) {
    if !policy.scan_secrets {
        return (input.to_string(), false);
    }
    let (result, count) = crate::scrub::scan_for_secrets(input);
    (result, count > 0)
}

/// Apply the redaction policy to a message, returning the display body
/// and whether it was redacted.
fn apply_body_redaction(
    body: &str,
    policy: &ExportRedactionPolicy,
    audit: &mut RedactionAuditLog,
    msg_id: i64,
) -> (String, bool) {
    // If no redaction policy is active, pass through unchanged.
    if !policy.is_active() {
        return (body.to_string(), false);
    }

    // If the body was already redacted by the scrub pass, preserve the placeholder.
    if is_redacted_body(body) {
        audit.record(
            RedactionReason::ScrubPreset,
            format!("message {msg_id}: body was redacted by scrub pass"),
            Some(msg_id),
        );
        return (BODY_REDACTED_PLACEHOLDER.to_string(), true);
    }

    // If the policy says to redact all bodies (strict mode), replace it.
    if policy.redact_bodies {
        audit.record(
            RedactionReason::BodyRedacted,
            format!("message {msg_id}: body hidden per strict export policy"),
            Some(msg_id),
        );
        return (policy.body_placeholder.clone(), true);
    }

    // Defense-in-depth: scan for any remaining secrets.
    let (sanitized, had_secrets) = defense_scan(body, policy);
    if had_secrets {
        audit.record(
            RedactionReason::SecretDetected,
            format!("message {msg_id}: secret pattern detected in body"),
            Some(msg_id),
        );
    }
    (sanitized, had_secrets)
}

/// Apply redaction to a subject line (defense-in-depth scanning only).
fn apply_subject_redaction(
    subject: &str,
    policy: &ExportRedactionPolicy,
    audit: &mut RedactionAuditLog,
    msg_id: i64,
) -> String {
    let (sanitized, had_secrets) = defense_scan(subject, policy);
    if had_secrets {
        audit.record(
            RedactionReason::SecretDetected,
            format!("message {msg_id}: secret pattern detected in subject"),
            Some(msg_id),
        );
    }
    sanitized
}

// ── Main pipeline ───────────────────────────────────────────────────────

/// Run the full static rendering pipeline.
///
/// Opens the exported database at `snapshot_path`, discovers all routes,
/// renders HTML pages, and writes them along with sitemap and search index
/// to `output_dir/viewer/pages/`.
///
/// The pipeline enforces the configured [`ExportRedactionPolicy`]:
/// - Defense-in-depth secret scanning on all rendered text
/// - Body redaction enforcement for strict presets
/// - Recipient list hiding for strict presets
/// - Audit log of all redaction actions taken
pub fn render_static_site(
    snapshot_path: &Path,
    output_dir: &Path,
    config: &StaticRenderConfig,
) -> ShareResult<StaticRenderResult> {
    let path_str = snapshot_path.display().to_string();
    let conn = DbConn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open snapshot for static render: {e}"),
    })?;

    let pages_dir = output_dir.join("viewer").join("pages");
    std::fs::create_dir_all(&pages_dir)?;

    let mut generated_files = Vec::new();
    let mut sitemap: Vec<SitemapEntry> = Vec::new();
    let mut search_index: Vec<SearchIndexEntry> = Vec::new();
    let mut audit = RedactionAuditLog::default();

    // ── Discover data ───────────────────────────────────────────────
    let projects = discover_projects(&conn)?;
    let raw_messages = discover_messages(&conn, config)?;

    // ── Apply redaction to all messages before any rendering ────────
    // This ensures all surfaces (project pages, inbox, thread views,
    // search index, sitemap) use redacted content consistently.
    let messages: Vec<MessageInfo> = raw_messages
        .iter()
        .map(|msg| {
            let (display_body, _body_redacted) =
                apply_body_redaction(&msg.body_md, &config.redaction, &mut audit, msg.id);
            let display_subject =
                apply_subject_redaction(&msg.subject, &config.redaction, &mut audit, msg.id);
            let display_recipients = if config.redaction.redact_recipients {
                if !msg.recipients.is_empty() {
                    audit.record(
                        RedactionReason::RecipientsHidden,
                        format!("message {}: recipients hidden", msg.id),
                        Some(msg.id),
                    );
                }
                Vec::new()
            } else {
                msg.recipients.clone()
            };
            MessageInfo {
                id: msg.id,
                subject: display_subject,
                body_md: display_body,
                importance: msg.importance.clone(),
                created_ts: msg.created_ts.clone(),
                sender_name: msg.sender_name.clone(),
                project_slug: msg.project_slug.clone(),
                thread_id: msg.thread_id.clone(),
                recipients: display_recipients,
            }
        })
        .collect();

    let threads = build_thread_index(&messages);

    // ── Render index page ───────────────────────────────────────────
    let index_html = render_index_page(&projects, config);
    write_page(&pages_dir, "index.html", &index_html)?;
    generated_files.push("viewer/pages/index.html".to_string());
    sitemap.push(SitemapEntry {
        route: "index.html".to_string(),
        title: format!("{} — Overview", config.site_title),
        parent: None,
        entry_type: "index".to_string(),
    });

    // ── Render projects list ────────────────────────────────────────
    let projects_html = render_projects_page(&projects, config);
    write_page(&pages_dir, "projects.html", &projects_html)?;
    generated_files.push("viewer/pages/projects.html".to_string());
    sitemap.push(SitemapEntry {
        route: "projects.html".to_string(),
        title: "Projects".to_string(),
        parent: Some("index.html".to_string()),
        entry_type: "projects".to_string(),
    });

    // ── Render per-project pages ────────────────────────────────────
    for project in &projects {
        let proj_dir = pages_dir.join("projects").join(&project.slug);
        std::fs::create_dir_all(&proj_dir)?;

        let proj_messages: Vec<&MessageInfo> = messages
            .iter()
            .filter(|m| m.project_slug == project.slug)
            .collect();

        let proj_html = render_project_page(project, &proj_messages, config);
        write_page(&proj_dir, "index.html", &proj_html)?;
        generated_files.push(format!("viewer/pages/projects/{}/index.html", project.slug));
        sitemap.push(SitemapEntry {
            route: format!("projects/{}/index.html", project.slug),
            title: format!("Project: {}", project.slug),
            parent: Some("projects.html".to_string()),
            entry_type: "project".to_string(),
        });

        // Render per-project inbox
        let inbox_html = render_inbox_page(project, &proj_messages, config);
        write_page(&proj_dir, "inbox.html", &inbox_html)?;
        generated_files.push(format!("viewer/pages/projects/{}/inbox.html", project.slug));
        sitemap.push(SitemapEntry {
            route: format!("projects/{}/inbox.html", project.slug),
            title: format!("Inbox: {}", project.slug),
            parent: Some(format!("projects/{}/index.html", project.slug)),
            entry_type: "inbox".to_string(),
        });
    }

    // ── Render per-message pages ────────────────────────────────────
    let msg_pages_dir = pages_dir.join("messages");
    std::fs::create_dir_all(&msg_pages_dir)?;

    for msg in &messages {
        let body_was_redacted = is_redacted_body(&msg.body_md);
        let msg_html = render_message_page(msg, body_was_redacted, config);
        let filename = format!("{}.html", msg.id);
        write_page(&msg_pages_dir, &filename, &msg_html)?;
        generated_files.push(format!("viewer/pages/messages/{filename}"));
        sitemap.push(SitemapEntry {
            route: format!("messages/{filename}"),
            title: msg.subject.clone(),
            parent: Some(format!("projects/{}/inbox.html", msg.project_slug)),
            entry_type: "message".to_string(),
        });

        // Build search index entry — exclude redacted bodies from snippets
        let snippet = if body_was_redacted {
            audit.record(
                RedactionReason::SnippetExcluded,
                format!("message {}: search snippet excluded", msg.id),
                Some(msg.id),
            );
            config.redaction.snippet_placeholder.clone()
        } else if msg.body_md.len() > config.search_snippet_len {
            let end = find_char_boundary(&msg.body_md, config.search_snippet_len);
            format!("{}...", &msg.body_md[..end])
        } else {
            msg.body_md.clone()
        };

        search_index.push(SearchIndexEntry {
            id: msg.id,
            subject: msg.subject.clone(),
            snippet,
            project: msg.project_slug.clone(),
            thread_id: msg.thread_id.clone(),
            sender: Some(msg.sender_name.clone()),
            importance: msg.importance.clone(),
            created_ts: msg.created_ts.clone(),
            route: format!("messages/{filename}"),
        });
    }

    // ── Render per-thread pages ─────────────────────────────────────
    let thread_pages_dir = pages_dir.join("threads");
    std::fs::create_dir_all(&thread_pages_dir)?;

    for (tid, info) in &threads {
        let thread_messages: Vec<&MessageInfo> = messages
            .iter()
            .filter(|m| m.thread_id.as_deref() == Some(tid.as_str()))
            .collect();

        let thread_html = render_thread_page(info, &thread_messages, config);
        let safe_id = sanitize_filename(tid);
        let filename = format!("{safe_id}.html");
        write_page(&thread_pages_dir, &filename, &thread_html)?;
        generated_files.push(format!("viewer/pages/threads/{filename}"));
        sitemap.push(SitemapEntry {
            route: format!("threads/{filename}"),
            title: format!("Thread: {}", info.subject),
            parent: Some(format!("projects/{}/inbox.html", info.project_slug)),
            entry_type: "thread".to_string(),
        });
    }

    // ── Write sitemap.json ──────────────────────────────────────────
    let data_dir = output_dir.join("viewer").join("data");
    std::fs::create_dir_all(&data_dir)?;

    let sitemap_json = serde_json::to_string_pretty(&sitemap).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(data_dir.join("sitemap.json"), &sitemap_json)?;
    generated_files.push("viewer/data/sitemap.json".to_string());

    // ── Write search_index.json ─────────────────────────────────────
    // Sort by id for determinism
    let mut sorted_index = search_index.clone();
    sorted_index.sort_by_key(|e| e.id);

    let search_json =
        serde_json::to_string_pretty(&sorted_index).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(data_dir.join("search_index.json"), &search_json)?;
    generated_files.push("viewer/data/search_index.json".to_string());

    // ── Write navigation.json ───────────────────────────────────────
    let nav = build_navigation(&projects, &threads);
    let nav_json = serde_json::to_string_pretty(&nav).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(data_dir.join("navigation.json"), &nav_json)?;
    generated_files.push("viewer/data/navigation.json".to_string());

    generated_files.sort();

    // ── Write redaction audit log ──────────────────────────────────
    if audit.total() > 0 {
        let audit_json = serde_json::to_string_pretty(&audit).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(data_dir.join("redaction_audit.json"), &audit_json)?;
        generated_files.push("viewer/data/redaction_audit.json".to_string());
        generated_files.sort();
    }

    Ok(StaticRenderResult {
        pages_generated: sitemap.len(),
        projects_count: projects.len(),
        messages_count: messages.len(),
        threads_count: threads.len(),
        search_index_entries: sorted_index.len(),
        generated_files,
        redaction_audit: audit,
    })
}

// ── Data discovery ──────────────────────────────────────────────────────

fn discover_projects(conn: &DbConn) -> ShareResult<Vec<ProjectInfo>> {
    let rows = conn
        .query_sync(
            "SELECT p.slug, p.human_key, \
             (SELECT COUNT(*) FROM messages m WHERE m.project_id = p.id) AS msg_count, \
             (SELECT COUNT(*) FROM agents a WHERE a.project_id = p.id) AS agent_count \
             FROM projects p ORDER BY p.slug",
            &[],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("discover projects: {e}"),
        })?;

    let mut projects = Vec::new();
    for row in &rows {
        projects.push(ProjectInfo {
            slug: row.get_named("slug").unwrap_or_default(),
            human_key: row.get_named("human_key").unwrap_or_default(),
            message_count: row.get_named("msg_count").unwrap_or(0),
            agent_count: row.get_named("agent_count").unwrap_or(0),
        });
    }
    Ok(projects)
}

fn discover_messages(conn: &DbConn, config: &StaticRenderConfig) -> ShareResult<Vec<MessageInfo>> {
    // Fetch messages joined with sender agent and project
    let rows = conn
        .query_sync(
            "SELECT m.id, m.subject, m.body_md, m.importance, m.created_ts, \
             m.thread_id, \
             COALESCE(a.name, 'unknown') AS sender_name, \
             p.slug AS project_slug \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             JOIN projects p ON p.id = m.project_id \
             ORDER BY m.created_ts ASC, m.id ASC",
            &[],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("discover messages: {e}"),
        })?;

    let mut messages = Vec::new();
    for row in &rows {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let body_md: String = row.get_named("body_md").unwrap_or_default();

        // Truncate body for non-body-included exports
        let body = if config.include_bodies {
            body_md
        } else {
            let end = find_char_boundary(&body_md, config.search_snippet_len);
            if end < body_md.len() {
                format!("{}...", &body_md[..end])
            } else {
                body_md
            }
        };

        let created_ts_raw: String = row.get_named("created_ts").unwrap_or_default();
        let created_ts = normalize_timestamp(&created_ts_raw);

        let thread_id: Option<String> = row.get_named("thread_id").ok();

        // Fetch recipients for this message
        let recipients = fetch_recipients(conn, id);

        messages.push(MessageInfo {
            id,
            subject: row.get_named("subject").unwrap_or_default(),
            body_md: body,
            importance: row.get_named("importance").unwrap_or_default(),
            created_ts,
            sender_name: row.get_named("sender_name").unwrap_or_default(),
            project_slug: row.get_named("project_slug").unwrap_or_default(),
            thread_id,
            recipients,
        });
    }
    Ok(messages)
}

fn fetch_recipients(conn: &DbConn, message_id: i64) -> Vec<String> {
    conn.query_sync(
        "SELECT a.name FROM message_recipients r \
         JOIN agents a ON a.id = r.agent_id \
         WHERE r.message_id = ?1 \
         ORDER BY a.name",
        &[SqlValue::BigInt(message_id)],
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|r| r.get_named::<String>("name").ok())
    .collect()
}

fn build_thread_index(messages: &[MessageInfo]) -> BTreeMap<String, ThreadInfo> {
    let mut threads: BTreeMap<String, ThreadInfo> = BTreeMap::new();

    for msg in messages {
        let Some(tid) = &msg.thread_id else {
            continue;
        };
        let entry = threads.entry(tid.clone()).or_insert_with(|| ThreadInfo {
            thread_id: tid.clone(),
            project_slug: msg.project_slug.clone(),
            subject: msg.subject.clone(),
            message_count: 0,
            participants: BTreeSet::new(),
            latest_ts: String::new(),
        });
        entry.message_count += 1;
        entry.participants.insert(msg.sender_name.clone());
        for r in &msg.recipients {
            entry.participants.insert(r.clone());
        }
        if msg.created_ts > entry.latest_ts {
            entry.latest_ts.clone_from(&msg.created_ts);
        }
    }
    threads
}

// ── HTML rendering helpers ──────────────────────────────────────────────

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

fn page_wrapper(
    title: &str,
    breadcrumbs: &[(&str, &str)],
    body: &str,
    config: &StaticRenderConfig,
) -> String {
    let mut crumbs = String::new();
    for (i, (label, href)) in breadcrumbs.iter().enumerate() {
        if i > 0 {
            crumbs.push_str(" &raquo; ");
        }
        if href.is_empty() {
            crumbs.push_str(&html_escape(label));
        } else {
            crumbs.push_str(&format!(
                "<a href=\"{}\">{}</a>",
                html_escape(href),
                html_escape(label)
            ));
        }
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title} — {site_title}</title>
  <style>
    :root {{ --bg: #0d1117; --fg: #c9d1d9; --accent: #58a6ff; --border: #30363d; --card: #161b22; }}
    * {{ margin: 0; padding: 0; box-sizing: border-box; }}
    body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; background: var(--bg); color: var(--fg); line-height: 1.6; padding: 2rem; max-width: 960px; margin: 0 auto; }}
    a {{ color: var(--accent); text-decoration: none; }} a:hover {{ text-decoration: underline; }}
    nav.breadcrumb {{ font-size: 0.85rem; margin-bottom: 1rem; color: #8b949e; }}
    h1 {{ font-size: 1.5rem; margin-bottom: 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }}
    h2 {{ font-size: 1.2rem; margin: 1.5rem 0 0.75rem; }}
    .card {{ background: var(--card); border: 1px solid var(--border); border-radius: 6px; padding: 1rem; margin-bottom: 0.75rem; }}
    .card h3 {{ font-size: 1rem; margin-bottom: 0.5rem; }}
    .meta {{ font-size: 0.8rem; color: #8b949e; }}
    .badge {{ display: inline-block; padding: 0.1rem 0.5rem; border-radius: 3px; font-size: 0.75rem; font-weight: 600; }}
    .badge-high {{ background: #da3633; color: #fff; }}
    .badge-normal {{ background: #30363d; color: #c9d1d9; }}
    .badge-low {{ background: #1f6feb33; color: #58a6ff; }}
    .body {{ margin-top: 0.75rem; white-space: pre-wrap; font-size: 0.9rem; }}
    table {{ width: 100%; border-collapse: collapse; margin-top: 0.5rem; }}
    th, td {{ text-align: left; padding: 0.5rem; border-bottom: 1px solid var(--border); font-size: 0.9rem; }}
    th {{ color: #8b949e; font-weight: 600; }}
    .stats {{ display: flex; gap: 2rem; margin: 1rem 0; }}
    .stat {{ text-align: center; }}
    .stat-value {{ font-size: 1.5rem; font-weight: 700; color: var(--accent); }}
    .stat-label {{ font-size: 0.75rem; color: #8b949e; }}
    footer {{ margin-top: 2rem; padding-top: 1rem; border-top: 1px solid var(--border); font-size: 0.8rem; color: #484f58; }}
  </style>
</head>
<body>
  <nav class="breadcrumb">{crumbs}</nav>
  <h1>{title_escaped}</h1>
  {body}
  <footer>Generated by MCP Agent Mail static export pipeline</footer>
</body>
</html>"#,
        title = html_escape(title),
        site_title = html_escape(&config.site_title),
        title_escaped = html_escape(title),
        crumbs = crumbs,
        body = body,
    )
}

fn importance_badge(importance: &str) -> String {
    let class = match importance {
        "high" | "urgent" => "badge-high",
        "low" => "badge-low",
        _ => "badge-normal",
    };
    format!(
        "<span class=\"badge {class}\">{}</span>",
        html_escape(importance)
    )
}

// ── Page renderers ──────────────────────────────────────────────────────

fn render_index_page(projects: &[ProjectInfo], config: &StaticRenderConfig) -> String {
    let total_messages: i64 = projects.iter().map(|p| p.message_count).sum();
    let total_agents: i64 = projects.iter().map(|p| p.agent_count).sum();

    let body = format!(
        r#"<div class="stats">
  <div class="stat"><div class="stat-value">{}</div><div class="stat-label">Projects</div></div>
  <div class="stat"><div class="stat-value">{}</div><div class="stat-label">Messages</div></div>
  <div class="stat"><div class="stat-value">{}</div><div class="stat-label">Agents</div></div>
</div>
<h2>Projects</h2>
<table>
  <thead><tr><th>Project</th><th>Messages</th><th>Agents</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
        projects.len(),
        total_messages,
        total_agents,
        rows = projects
            .iter()
            .map(|p| format!(
                "<tr><td><a href=\"projects/{slug}/index.html\">{slug}</a></td><td>{msgs}</td><td>{agents}</td></tr>",
                slug = html_escape(&p.slug),
                msgs = p.message_count,
                agents = p.agent_count,
            ))
            .collect::<Vec<_>>()
            .join("\n    "),
    );

    page_wrapper(&config.site_title, &[("Home", "")], &body, config)
}

fn render_projects_page(projects: &[ProjectInfo], config: &StaticRenderConfig) -> String {
    let body = format!(
        r#"<table>
  <thead><tr><th>Slug</th><th>Path</th><th>Messages</th><th>Agents</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
        rows = projects
            .iter()
            .map(|p| format!(
                "<tr><td><a href=\"projects/{slug}/index.html\">{slug}</a></td><td class=\"meta\">{key}</td><td>{msgs}</td><td>{agents}</td></tr>",
                slug = html_escape(&p.slug),
                key = html_escape(&p.human_key),
                msgs = p.message_count,
                agents = p.agent_count,
            ))
            .collect::<Vec<_>>()
            .join("\n    "),
    );

    page_wrapper(
        "All Projects",
        &[("Home", "index.html"), ("Projects", "")],
        &body,
        config,
    )
}

fn render_project_page(
    project: &ProjectInfo,
    messages: &[&MessageInfo],
    config: &StaticRenderConfig,
) -> String {
    let recent: Vec<&&MessageInfo> = messages.iter().rev().take(20).collect();
    let body = format!(
        r#"<div class="stats">
  <div class="stat"><div class="stat-value">{msgs}</div><div class="stat-label">Messages</div></div>
  <div class="stat"><div class="stat-value">{agents}</div><div class="stat-label">Agents</div></div>
</div>
<p class="meta">Path: {key}</p>
<p><a href="inbox.html">View full inbox &rarr;</a></p>
<h2>Recent Messages</h2>
{rows}"#,
        msgs = project.message_count,
        agents = project.agent_count,
        key = html_escape(&project.human_key),
        rows = recent
            .iter()
            .map(|m| format!(
                "<div class=\"card\"><h3><a href=\"../../messages/{id}.html\">{subj}</a></h3>\
                 <div class=\"meta\">{sender} &middot; {ts} {badge}</div></div>",
                id = m.id,
                subj = html_escape(&m.subject),
                sender = html_escape(&m.sender_name),
                ts = html_escape(&m.created_ts),
                badge = importance_badge(&m.importance),
            ))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    page_wrapper(
        &format!("Project: {}", project.slug),
        &[
            ("Home", "../../index.html"),
            ("Projects", "../../projects.html"),
            (&project.slug, ""),
        ],
        &body,
        config,
    )
}

fn render_inbox_page(
    project: &ProjectInfo,
    messages: &[&MessageInfo],
    config: &StaticRenderConfig,
) -> String {
    let display_msgs: Vec<&&MessageInfo> = messages
        .iter()
        .rev()
        .take(config.messages_per_page)
        .collect();

    let body = format!(
        r#"<p class="meta">{total} messages total (showing up to {limit})</p>
<table>
  <thead><tr><th>Subject</th><th>From</th><th>Date</th><th>Importance</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
        total = messages.len(),
        limit = config.messages_per_page,
        rows = display_msgs
            .iter()
            .map(|m| format!(
                "<tr><td><a href=\"../../messages/{id}.html\">{subj}</a></td>\
                 <td>{sender}</td><td class=\"meta\">{ts}</td><td>{badge}</td></tr>",
                id = m.id,
                subj = html_escape(&m.subject),
                sender = html_escape(&m.sender_name),
                ts = html_escape(&m.created_ts),
                badge = importance_badge(&m.importance),
            ))
            .collect::<Vec<_>>()
            .join("\n    "),
    );

    page_wrapper(
        &format!("Inbox: {}", project.slug),
        &[
            ("Home", "../../index.html"),
            ("Projects", "../../projects.html"),
            (&project.slug, "index.html"),
            ("Inbox", ""),
        ],
        &body,
        config,
    )
}

fn render_message_page(
    msg: &MessageInfo,
    body_was_redacted: bool,
    config: &StaticRenderConfig,
) -> String {
    let thread_link = msg
        .thread_id
        .as_ref()
        .map(|tid| {
            let safe_id = sanitize_filename(tid);
            format!(
                "<p>Thread: <a href=\"../threads/{safe_id}.html\">{tid}</a></p>",
                tid = html_escape(tid),
            )
        })
        .unwrap_or_default();

    let recipients_str = if config.redaction.redact_recipients {
        "<p class=\"meta redaction-notice\" data-redaction-reason=\"recipients_hidden\">\
         To: <em>[Recipients hidden per export policy]</em></p>"
            .to_string()
    } else if msg.recipients.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"meta\">To: {}</p>",
            html_escape(&msg.recipients.join(", "))
        )
    };

    // Render body with redaction notice if applicable
    let body_html = if body_was_redacted {
        format!(
            "<div class=\"body redaction-notice\" data-redaction-reason=\"body_redacted\">\
             <em>{}</em></div>",
            html_escape(&msg.body_md)
        )
    } else {
        format!("<div class=\"body\">{}</div>", html_escape(&msg.body_md))
    };

    let body = format!(
        r#"<div class="meta">
  <p>From: <strong>{sender}</strong></p>
  {recipients}
  <p>Project: <a href="../projects/{project}/index.html">{project}</a></p>
  <p>Date: {ts}</p>
  <p>Importance: {badge}</p>
  {thread_link}
</div>
{body_html}"#,
        sender = html_escape(&msg.sender_name),
        recipients = recipients_str,
        project = html_escape(&msg.project_slug),
        ts = html_escape(&msg.created_ts),
        badge = importance_badge(&msg.importance),
        thread_link = thread_link,
        body_html = body_html,
    );

    page_wrapper(
        &msg.subject,
        &[
            ("Home", "../index.html"),
            (
                &msg.project_slug,
                &format!("../projects/{}/index.html", msg.project_slug),
            ),
            ("Message", ""),
        ],
        &body,
        config,
    )
}

fn render_thread_page(
    info: &ThreadInfo,
    messages: &[&MessageInfo],
    config: &StaticRenderConfig,
) -> String {
    let participants: Vec<String> = info.participants.iter().cloned().collect();
    let body = format!(
        r#"<div class="meta">
  <p>Project: <a href="../projects/{project}/index.html">{project}</a></p>
  <p>Messages: {count}</p>
  <p>Participants: {participants}</p>
  <p>Latest: {latest}</p>
</div>
<h2>Messages in Thread</h2>
{cards}"#,
        project = html_escape(&info.project_slug),
        count = info.message_count,
        participants = html_escape(&participants.join(", ")),
        latest = html_escape(&info.latest_ts),
        cards = messages
            .iter()
            .map(|m| {
                // Apply redaction to body snippets in thread view
                let body_snippet = if config.redaction.redact_bodies || is_redacted_body(&m.body_md)
                {
                    format!("<em>{}</em>", html_escape(BODY_REDACTED_PLACEHOLDER))
                } else {
                    let truncated = truncate_str(&m.body_md, 500);
                    let (sanitized, _) = defense_scan(&truncated, &config.redaction);
                    html_escape(&sanitized)
                };
                format!(
                    "<div class=\"card\"><h3><a href=\"../messages/{id}.html\">{subj}</a></h3>\
                     <div class=\"meta\">{sender} &middot; {ts}</div>\
                     <div class=\"body\">{body}</div></div>",
                    id = m.id,
                    subj = html_escape(&m.subject),
                    sender = html_escape(&m.sender_name),
                    ts = html_escape(&m.created_ts),
                    body = body_snippet,
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );

    page_wrapper(
        &format!("Thread: {}", info.subject),
        &[
            ("Home", "../index.html"),
            (
                &info.project_slug,
                &format!("../projects/{}/index.html", info.project_slug),
            ),
            ("Thread", ""),
        ],
        &body,
        config,
    )
}

// ── Navigation structure ────────────────────────────────────────────────

fn build_navigation(
    projects: &[ProjectInfo],
    threads: &BTreeMap<String, ThreadInfo>,
) -> serde_json::Value {
    let project_entries: Vec<serde_json::Value> = projects
        .iter()
        .map(|p| {
            serde_json::json!({
                "slug": p.slug,
                "human_key": p.human_key,
                "message_count": p.message_count,
                "agent_count": p.agent_count,
                "routes": {
                    "overview": format!("projects/{}/index.html", p.slug),
                    "inbox": format!("projects/{}/inbox.html", p.slug),
                }
            })
        })
        .collect();

    let thread_entries: Vec<serde_json::Value> = threads
        .values()
        .map(|t| {
            let safe_id = sanitize_filename(&t.thread_id);
            serde_json::json!({
                "thread_id": t.thread_id,
                "project": t.project_slug,
                "subject": t.subject,
                "message_count": t.message_count,
                "participants": t.participants.iter().collect::<Vec<_>>(),
                "route": format!("threads/{safe_id}.html"),
            })
        })
        .collect();

    serde_json::json!({
        "projects": project_entries,
        "threads": thread_entries,
        "entry_point": "index.html",
    })
}

// ── Utility helpers ─────────────────────────────────────────────────────

fn write_page(dir: &Path, filename: &str, content: &str) -> ShareResult<()> {
    let path = dir.join(filename);
    std::fs::write(&path, content)?;
    Ok(())
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut boundary = target;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let end = find_char_boundary(s, max_len);
    format!("{}...", &s[..end])
}

fn normalize_timestamp(ts: &str) -> String {
    // If it looks like a microsecond integer, convert to ISO-8601
    if let Ok(micros) = ts.parse::<i64>() {
        let secs = micros.div_euclid(1_000_000);
        let sub_micros = micros.rem_euclid(1_000_000);
        let nanos = (sub_micros * 1000) as u32;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
            return dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        }
    }
    // Already a string timestamp
    ts.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── html_escape ─────────────────────────────────────────────────

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(
            html_escape("<script>&'\""),
            "&lt;script&gt;&amp;&#x27;&quot;"
        );
    }

    #[test]
    fn html_escape_plain_text() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    // ── sanitize_filename ───────────────────────────────────────────

    #[test]
    fn sanitize_filename_preserves_safe_chars() {
        assert_eq!(sanitize_filename("abc-123_test.html"), "abc-123_test.html");
    }

    #[test]
    fn sanitize_filename_replaces_special() {
        assert_eq!(sanitize_filename("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn sanitize_filename_handles_spaces() {
        assert_eq!(sanitize_filename("my thread id"), "my_thread_id");
    }

    // ── normalize_timestamp ─────────────────────────────────────────

    #[test]
    fn normalize_timestamp_micros() {
        let result = normalize_timestamp("1707000000000000");
        assert!(result.starts_with("2024-02-0"));
        assert!(result.ends_with('Z'));
    }

    #[test]
    fn normalize_timestamp_iso_passthrough() {
        let ts = "2024-02-03T12:00:00Z";
        assert_eq!(normalize_timestamp(ts), ts);
    }

    // ── truncate_str ────────────────────────────────────────────────

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        let result = truncate_str("hello world this is long", 10);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 13); // 10 + "..."
    }

    // ── find_char_boundary ──────────────────────────────────────────

    #[test]
    fn find_char_boundary_ascii() {
        assert_eq!(find_char_boundary("hello", 3), 3);
    }

    #[test]
    fn find_char_boundary_beyond_len() {
        assert_eq!(find_char_boundary("hi", 10), 2);
    }

    // ── importance_badge ────────────────────────────────────────────

    #[test]
    fn importance_badge_high() {
        let badge = importance_badge("high");
        assert!(badge.contains("badge-high"));
    }

    #[test]
    fn importance_badge_normal() {
        let badge = importance_badge("normal");
        assert!(badge.contains("badge-normal"));
    }

    // ── page_wrapper ────────────────────────────────────────────────

    #[test]
    fn page_wrapper_contains_title() {
        let config = StaticRenderConfig::default();
        let html = page_wrapper(
            "Test Page",
            &[("Home", "index.html")],
            "<p>Hello</p>",
            &config,
        );
        assert!(html.contains("Test Page"));
        assert!(html.contains("<p>Hello</p>"));
        assert!(html.contains("<!doctype html>"));
    }

    #[test]
    fn page_wrapper_breadcrumbs() {
        let config = StaticRenderConfig::default();
        let html = page_wrapper(
            "Test",
            &[
                ("Home", "index.html"),
                ("Projects", "projects.html"),
                ("Current", ""),
            ],
            "",
            &config,
        );
        assert!(html.contains("<a href=\"index.html\">Home</a>"));
        assert!(html.contains("<a href=\"projects.html\">Projects</a>"));
        assert!(html.contains("Current")); // No link for empty href
    }

    // ── build_thread_index ──────────────────────────────────────────

    #[test]
    fn build_thread_index_groups_by_thread() {
        let messages = vec![
            MessageInfo {
                id: 1,
                subject: "Hello".to_string(),
                body_md: "body".to_string(),
                importance: "normal".to_string(),
                created_ts: "2024-01-01T00:00:00Z".to_string(),
                sender_name: "Alice".to_string(),
                project_slug: "proj".to_string(),
                thread_id: Some("t1".to_string()),
                recipients: vec!["Bob".to_string()],
            },
            MessageInfo {
                id: 2,
                subject: "Re: Hello".to_string(),
                body_md: "reply".to_string(),
                importance: "normal".to_string(),
                created_ts: "2024-01-01T01:00:00Z".to_string(),
                sender_name: "Bob".to_string(),
                project_slug: "proj".to_string(),
                thread_id: Some("t1".to_string()),
                recipients: vec!["Alice".to_string()],
            },
            MessageInfo {
                id: 3,
                subject: "Other".to_string(),
                body_md: "unrelated".to_string(),
                importance: "high".to_string(),
                created_ts: "2024-01-02T00:00:00Z".to_string(),
                sender_name: "Charlie".to_string(),
                project_slug: "proj".to_string(),
                thread_id: None,
                recipients: vec![],
            },
        ];

        let threads = build_thread_index(&messages);
        assert_eq!(threads.len(), 1);
        let t1 = &threads["t1"];
        assert_eq!(t1.message_count, 2);
        assert!(t1.participants.contains("Alice"));
        assert!(t1.participants.contains("Bob"));
        assert_eq!(t1.latest_ts, "2024-01-01T01:00:00Z");
    }

    // ── SearchIndexEntry serialization ──────────────────────────────

    #[test]
    fn search_index_entry_serializes() {
        let entry = SearchIndexEntry {
            id: 1,
            subject: "Test".to_string(),
            snippet: "body".to_string(),
            project: "proj".to_string(),
            thread_id: None,
            sender: Some("Alice".to_string()),
            importance: "normal".to_string(),
            created_ts: "2024-01-01T00:00:00Z".to_string(),
            route: "messages/1.html".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"subject\":\"Test\""));
        assert!(!json.contains("thread_id")); // skip_serializing_if None
    }

    // ── StaticRenderConfig defaults ─────────────────────────────────

    #[test]
    fn config_defaults_are_reasonable() {
        let config = StaticRenderConfig::default();
        assert_eq!(config.messages_per_page, 200);
        assert_eq!(config.search_snippet_len, 300);
        assert!(config.include_bodies);
        assert!(config.redaction.scan_secrets);
        assert!(!config.redaction.redact_bodies);
    }

    // ── Redaction policy helpers ──────────────────────────────────────

    #[test]
    fn is_redacted_body_detects_placeholder() {
        assert!(is_redacted_body("[Message body redacted]"));
        assert!(is_redacted_body("  [Message body redacted]  "));
        assert!(!is_redacted_body("Normal message body"));
    }

    #[test]
    fn defense_scan_catches_github_token() {
        let policy = ExportRedactionPolicy::default();
        let (result, found) = defense_scan("Use ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789", &policy);
        assert!(found);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_"));
    }

    #[test]
    fn defense_scan_noop_when_disabled() {
        let policy = ExportRedactionPolicy::none();
        let input = "Use ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        let (result, found) = defense_scan(input, &policy);
        assert!(!found);
        assert_eq!(result, input);
    }

    // ── Integration: render with in-memory DB ───────────────────────

    #[test]
    fn render_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");

        // Create minimal schema
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT, body_md TEXT, importance TEXT, created_ts TEXT, thread_id TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, agent_id INTEGER, \
             read_ts TEXT, ack_ts TEXT)",
            &[],
        )
        .unwrap();
        drop(conn);

        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        let result = render_static_site(&db_path, &output, &config).unwrap();

        assert_eq!(result.projects_count, 0);
        assert_eq!(result.messages_count, 0);
        assert_eq!(result.threads_count, 0);
        assert!(result.pages_generated > 0); // index + projects pages
        assert!(output.join("viewer/pages/index.html").exists());
        assert!(output.join("viewer/pages/projects.html").exists());
        assert!(output.join("viewer/data/sitemap.json").exists());
        assert!(output.join("viewer/data/search_index.json").exists());
        assert!(output.join("viewer/data/navigation.json").exists());
    }

    #[test]
    fn render_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT, body_md TEXT, importance TEXT, created_ts TEXT, thread_id TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, agent_id INTEGER, \
             read_ts TEXT, ack_ts TEXT)",
            &[],
        )
        .unwrap();

        // Insert test data
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'test-project', '/tmp/test')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'RedFox')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (2, 1, 'BlueLake')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, created_ts, thread_id) \
             VALUES (1, 1, 1, 'Hello World', 'This is a test message body.', 'normal', '2024-01-01T00:00:00Z', 'thread-1')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, created_ts, thread_id) \
             VALUES (2, 1, 2, 'Re: Hello World', 'Reply to the test message.', 'high', '2024-01-01T01:00:00Z', 'thread-1')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id) VALUES (1, 1, 2)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id) VALUES (2, 2, 1)",
            &[],
        )
        .unwrap();
        drop(conn);

        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        let result = render_static_site(&db_path, &output, &config).unwrap();

        assert_eq!(result.projects_count, 1);
        assert_eq!(result.messages_count, 2);
        assert_eq!(result.threads_count, 1);
        assert_eq!(result.search_index_entries, 2);

        // Check generated files exist
        assert!(output.join("viewer/pages/index.html").exists());
        assert!(
            output
                .join("viewer/pages/projects/test-project/index.html")
                .exists()
        );
        assert!(
            output
                .join("viewer/pages/projects/test-project/inbox.html")
                .exists()
        );
        assert!(output.join("viewer/pages/messages/1.html").exists());
        assert!(output.join("viewer/pages/messages/2.html").exists());
        assert!(output.join("viewer/pages/threads/thread-1.html").exists());

        // Check search index content
        let search_json =
            std::fs::read_to_string(output.join("viewer/data/search_index.json")).unwrap();
        let entries: Vec<SearchIndexEntry> = serde_json::from_str(&search_json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[1].id, 2);

        // Check sitemap
        let sitemap_json =
            std::fs::read_to_string(output.join("viewer/data/sitemap.json")).unwrap();
        let sitemap: Vec<SitemapEntry> = serde_json::from_str(&sitemap_json).unwrap();
        assert!(sitemap.len() >= 7); // index + projects + 1 project + inbox + 2 messages + 1 thread

        // Check navigation
        let nav_json = std::fs::read_to_string(output.join("viewer/data/navigation.json")).unwrap();
        let nav: serde_json::Value = serde_json::from_str(&nav_json).unwrap();
        assert_eq!(nav["projects"].as_array().unwrap().len(), 1);
        assert_eq!(nav["threads"].as_array().unwrap().len(), 1);

        // Check message HTML content
        let msg_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/1.html")).unwrap();
        assert!(msg_html.contains("Hello World"));
        assert!(msg_html.contains("RedFox"));
        assert!(msg_html.contains("test-project"));
    }

    // ── Threat-model negative fixture tests ──────────────────────────
    //
    // These tests verify that secrets and sensitive content never leak
    // through any export artifact (HTML, search index, sitemap, navigation).

    /// Helper: create a fixture DB with messages containing secrets.
    fn create_secret_fixture_db(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("secrets.sqlite3");
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT, body_md TEXT, importance TEXT, created_ts TEXT, thread_id TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, agent_id INTEGER, \
             read_ts TEXT, ack_ts TEXT)",
            &[],
        )
        .unwrap();

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'proj', '/tmp/p')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'RedFox')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (2, 1, 'BlueLake')",
            &[],
        )
        .unwrap();

        // Message 1: GitHub token in body
        conn.execute_sync(
            "INSERT INTO messages VALUES (1, 1, 1, 'Setup instructions', \
             'Use token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 for auth', \
             'normal', '2024-01-01T00:00:00Z', 'setup-thread')",
            &[],
        )
        .unwrap();

        // Message 2: AWS key in subject
        conn.execute_sync(
            "INSERT INTO messages VALUES (2, 1, 2, 'AWS key AKIAIOSFODNN7EXAMPLE found in config', \
             'Please rotate this key immediately.', \
             'high', '2024-01-01T01:00:00Z', 'setup-thread')",
            &[],
        )
        .unwrap();

        // Message 3: Bearer token in body
        conn.execute_sync(
            "INSERT INTO messages VALUES (3, 1, 1, 'API integration', \
             'Set header: bearer MySecretTokenABCDEF1234567890', \
             'normal', '2024-01-02T00:00:00Z', NULL)",
            &[],
        )
        .unwrap();

        // Message 4: Already-redacted body (simulating scrub pass)
        conn.execute_sync(
            "INSERT INTO messages VALUES (4, 1, 2, 'Credential rotation', \
             '[Message body redacted]', \
             'normal', '2024-01-03T00:00:00Z', 'setup-thread')",
            &[],
        )
        .unwrap();

        // Message 5: JWT in body
        conn.execute_sync(
            "INSERT INTO messages VALUES (5, 1, 1, 'Session token', \
             'Token: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U', \
             'normal', '2024-01-04T00:00:00Z', NULL)",
            &[],
        )
        .unwrap();

        // Add recipients (id, message_id, agent_id, read_ts, ack_ts)
        conn.execute_sync(
            "INSERT INTO message_recipients VALUES (1, 1, 2, NULL, NULL)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients VALUES (2, 2, 1, NULL, NULL)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients VALUES (3, 3, 2, NULL, NULL)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients VALUES (4, 4, 1, NULL, NULL)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients VALUES (5, 5, 2, NULL, NULL)",
            &[],
        )
        .unwrap();
        drop(conn);
        db_path
    }

    /// Collect all text content from all generated files.
    fn collect_all_output_text(output_dir: &std::path::Path) -> String {
        let mut all_text = String::new();
        for entry in walkdir(output_dir) {
            if let Ok(content) = std::fs::read_to_string(&entry) {
                all_text.push_str(&content);
                all_text.push('\n');
            }
        }
        all_text
    }

    /// Simple directory walker (no external dep needed).
    fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    files.push(path);
                }
            }
        }
        files
    }

    /// THREAT: GitHub tokens must not appear in any generated HTML page.
    #[test]
    fn negative_github_token_not_in_html() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default(); // standard: scan_secrets=true

        let result = render_static_site(&db_path, &output, &config).unwrap();

        // The message page for msg 1 must not contain the raw token
        let msg1_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/1.html")).unwrap();
        assert!(
            !msg1_html.contains("ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            "GitHub token leaked into message HTML"
        );
        assert!(
            msg1_html.contains("[REDACTED]"),
            "Token should be replaced with [REDACTED]"
        );
        assert!(result.redaction_audit.secrets_caught > 0);
    }

    /// THREAT: AWS access key IDs must not appear in any generated output.
    #[test]
    fn negative_aws_key_not_in_any_output() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        render_static_site(&db_path, &output, &config).unwrap();

        let all_text = collect_all_output_text(&output);
        assert!(
            !all_text.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked into generated output"
        );
    }

    /// THREAT: Bearer tokens must not appear in search index.
    #[test]
    fn negative_bearer_token_not_in_search_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        render_static_site(&db_path, &output, &config).unwrap();

        let search_json =
            std::fs::read_to_string(output.join("viewer/data/search_index.json")).unwrap();
        assert!(
            !search_json.contains("MySecretTokenABCDEF1234567890"),
            "Bearer token leaked into search index"
        );
    }

    /// THREAT: JWTs must not appear in thread page body snippets.
    #[test]
    fn negative_jwt_not_in_thread_page() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        render_static_site(&db_path, &output, &config).unwrap();

        // The setup-thread page has messages with secrets
        let thread_html =
            std::fs::read_to_string(output.join("viewer/pages/threads/setup-thread.html")).unwrap();
        assert!(
            !thread_html.contains("eyJhbGciOiJIUzI1NiJ9"),
            "JWT fragment leaked into thread HTML"
        );
    }

    /// THREAT: Strict preset must redact ALL message bodies.
    #[test]
    fn negative_strict_preset_redacts_all_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");

        let config = StaticRenderConfig {
            redaction: ExportRedactionPolicy::from_preset(crate::ScrubPreset::Strict),
            ..StaticRenderConfig::default()
        };
        let result = render_static_site(&db_path, &output, &config).unwrap();

        // All message pages should contain the redaction notice
        let msg3_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/3.html")).unwrap();
        assert!(
            msg3_html.contains("data-redaction-reason"),
            "Strict preset message should have redaction reason attribute"
        );
        assert!(
            !msg3_html.contains("Set header:"),
            "Body content leaked despite strict preset"
        );

        // Search index should have placeholder snippets
        let search_json =
            std::fs::read_to_string(output.join("viewer/data/search_index.json")).unwrap();
        assert!(
            !search_json.contains("rotate this key"),
            "Body content leaked into search index under strict preset"
        );
        assert!(
            search_json.contains("Content hidden per export policy"),
            "Strict preset should use snippet placeholder"
        );

        // Audit log should reflect redaction counts
        assert!(result.redaction_audit.bodies_redacted > 0);
        assert!(result.redaction_audit.snippets_filtered > 0);
    }

    /// THREAT: Strict preset must hide recipient lists.
    #[test]
    fn negative_strict_preset_hides_recipients() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");

        let config = StaticRenderConfig {
            redaction: ExportRedactionPolicy::from_preset(crate::ScrubPreset::Strict),
            ..StaticRenderConfig::default()
        };
        let result = render_static_site(&db_path, &output, &config).unwrap();

        let msg1_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/1.html")).unwrap();
        assert!(
            msg1_html.contains("Recipients hidden per export policy"),
            "Strict preset should hide recipient names"
        );
        assert!(result.redaction_audit.recipients_hidden > 0);
    }

    /// THREAT: Already-redacted bodies must not be re-exposed.
    #[test]
    fn negative_pre_redacted_body_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        render_static_site(&db_path, &output, &config).unwrap();

        // Message 4 had "[Message body redacted]" in the DB
        let msg4_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/4.html")).unwrap();
        assert!(
            msg4_html.contains("data-redaction-reason=\"body_redacted\""),
            "Pre-redacted body should carry a redaction reason"
        );
        // Search index for msg 4 should have placeholder
        let search_json =
            std::fs::read_to_string(output.join("viewer/data/search_index.json")).unwrap();
        let entries: Vec<SearchIndexEntry> = serde_json::from_str(&search_json).unwrap();
        let msg4_entry = entries.iter().find(|e| e.id == 4).unwrap();
        assert!(
            msg4_entry
                .snippet
                .contains("Content hidden per export policy"),
            "Pre-redacted body should have placeholder snippet, got: {}",
            msg4_entry.snippet
        );
    }

    /// THREAT: Archive preset must NOT redact (preserves everything).
    #[test]
    fn negative_archive_preset_preserves_all() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");

        let config = StaticRenderConfig {
            redaction: ExportRedactionPolicy::none(),
            ..StaticRenderConfig::default()
        };
        let result = render_static_site(&db_path, &output, &config).unwrap();

        // With archive/none policy, secrets are NOT scanned
        assert_eq!(result.redaction_audit.secrets_caught, 0);
        assert_eq!(result.redaction_audit.bodies_redacted, 0);

        // Bodies should appear as-is (including secrets — this is archive mode)
        let msg1_html =
            std::fs::read_to_string(output.join("viewer/pages/messages/1.html")).unwrap();
        assert!(
            msg1_html.contains("ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            "Archive mode should preserve original content"
        );
    }

    /// THREAT: Redaction audit log must be generated when events occur.
    #[test]
    fn negative_audit_log_generated_with_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = create_secret_fixture_db(dir.path());
        let output = dir.path().join("output");
        let config = StaticRenderConfig::default();
        let result = render_static_site(&db_path, &output, &config).unwrap();

        assert!(result.redaction_audit.total() > 0);
        assert!(
            output.join("viewer/data/redaction_audit.json").exists(),
            "Audit log file should be generated when redaction events occur"
        );

        let audit_json =
            std::fs::read_to_string(output.join("viewer/data/redaction_audit.json")).unwrap();
        let audit: RedactionAuditLog = serde_json::from_str(&audit_json).unwrap();
        assert!(audit.total() > 0);
    }

    /// THREAT: No audit log file when archive mode (no events).
    #[test]
    fn negative_no_audit_log_in_archive_mode() {
        let dir = tempfile::tempdir().unwrap();

        // Create a DB without secrets
        let db_path = dir.path().join("clean.sqlite3");
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT, body_md TEXT, importance TEXT, created_ts TEXT, thread_id TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, agent_id INTEGER, \
             read_ts TEXT, ack_ts TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync("INSERT INTO projects VALUES (1, 'p', '/tmp/p')", &[])
            .unwrap();
        conn.execute_sync("INSERT INTO agents VALUES (1, 1, 'AgentA')", &[])
            .unwrap();
        conn.execute_sync(
            "INSERT INTO messages VALUES (1, 1, 1, 'Clean', 'No secrets here', 'normal', '2024-01-01T00:00:00Z', NULL)",
            &[],
        )
        .unwrap();
        drop(conn);

        let output = dir.path().join("output");
        let config = StaticRenderConfig {
            redaction: ExportRedactionPolicy::none(),
            ..StaticRenderConfig::default()
        };
        let result = render_static_site(&db_path, &output, &config).unwrap();

        assert_eq!(result.redaction_audit.total(), 0);
        assert!(
            !output.join("viewer/data/redaction_audit.json").exists(),
            "No audit log should be created when there are no redaction events"
        );
    }

    #[test]
    fn render_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT, body_md TEXT, importance TEXT, created_ts TEXT, thread_id TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, agent_id INTEGER, \
             read_ts TEXT, ack_ts TEXT)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'proj', '/tmp/p')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'Agent1')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, created_ts, thread_id) \
             VALUES (1, 1, 1, 'Test', 'Body', 'normal', '2024-01-01T00:00:00Z', NULL)",
            &[],
        )
        .unwrap();
        drop(conn);

        let config = StaticRenderConfig::default();

        // Render twice to different output dirs
        let out1 = dir.path().join("out1");
        let out2 = dir.path().join("out2");
        let r1 = render_static_site(&db_path, &out1, &config).unwrap();
        let r2 = render_static_site(&db_path, &out2, &config).unwrap();

        // Results should be identical
        assert_eq!(r1.generated_files, r2.generated_files);
        assert_eq!(r1.pages_generated, r2.pages_generated);

        // File contents should be byte-identical
        for file in &r1.generated_files {
            let c1 = std::fs::read_to_string(out1.join(file)).unwrap();
            let c2 = std::fs::read_to_string(out2.join(file)).unwrap();
            assert_eq!(c1, c2, "Files differ: {file}");
        }
    }
}
