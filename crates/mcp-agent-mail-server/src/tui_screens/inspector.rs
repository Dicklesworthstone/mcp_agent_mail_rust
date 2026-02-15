//! Inspector detail cards for the timeline pane.
//!
//! Renders structured detail views for selected `MailEvent` entries,
//! with masked payloads and copy-friendly formatting.
//!
//! ## Correlation Links
//!
//! Each event can reference entities (agents, threads, tools, projects,
//! messages) that are navigable via deep-link.  [`extract_links`] pulls
//! these references out of a `MailEvent` and returns them as numbered
//! [`CorrelationLink`] entries.  The inspector renders these with
//! number-key indicators so the operator can press `1`..`9` to navigate.

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Frame, Style};
use std::fmt::Write as _;

use super::DeepLinkTarget;
use super::dashboard::format_event;
use crate::tui_events::{MailEvent, MailEventKind};

// ──────────────────────────────────────────────────────────────────────
// Correlation links — extractable navigable references
// ──────────────────────────────────────────────────────────────────────

/// A navigable reference extracted from a `MailEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelationLink {
    /// Human-readable label (e.g. "Agent: `RedFox`").
    pub label: String,
    /// The deep-link target for navigation.
    pub target: DeepLinkTarget,
}

/// Extract all navigable correlation links from an event.
///
/// Links are returned in a stable order: project, agent(s), thread,
/// message, tool.  Duplicates are suppressed.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn extract_links(event: &MailEvent) -> Vec<CorrelationLink> {
    let mut links = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push = |label: String, target: DeepLinkTarget| {
        let key = format!("{target:?}");
        if seen.insert(key) {
            links.push(CorrelationLink { label, target });
        }
    };

    match event {
        MailEvent::ToolCallStart {
            tool_name,
            project,
            agent,
            ..
        }
        | MailEvent::ToolCallEnd {
            tool_name,
            project,
            agent,
            ..
        } => {
            if let Some(p) = project {
                push(
                    format!("Project: {p}"),
                    DeepLinkTarget::ProjectBySlug(p.clone()),
                );
            }
            if let Some(a) = agent {
                push(
                    format!("Agent: {a}"),
                    DeepLinkTarget::AgentByName(a.clone()),
                );
            }
            push(
                format!("Tool: {tool_name}"),
                DeepLinkTarget::ToolByName(tool_name.clone()),
            );
        }

        MailEvent::MessageSent {
            id,
            from,
            to,
            thread_id,
            project,
            ..
        }
        | MailEvent::MessageReceived {
            id,
            from,
            to,
            thread_id,
            project,
            ..
        } => {
            push(
                format!("Project: {project}"),
                DeepLinkTarget::ProjectBySlug(project.clone()),
            );
            push(
                format!("From: {from}"),
                DeepLinkTarget::AgentByName(from.clone()),
            );
            for recipient in to {
                push(
                    format!("To: {recipient}"),
                    DeepLinkTarget::AgentByName(recipient.clone()),
                );
            }
            push(
                format!("Thread: {thread_id}"),
                DeepLinkTarget::ThreadById(thread_id.clone()),
            );
            push(format!("Message: #{id}"), DeepLinkTarget::MessageById(*id));
        }

        MailEvent::ReservationGranted { agent, project, .. }
        | MailEvent::ReservationReleased { agent, project, .. } => {
            push(
                format!("Project: {project}"),
                DeepLinkTarget::ProjectBySlug(project.clone()),
            );
            push(
                format!("Agent: {agent}"),
                DeepLinkTarget::AgentByName(agent.clone()),
            );
        }

        MailEvent::AgentRegistered { name, project, .. } => {
            push(
                format!("Project: {project}"),
                DeepLinkTarget::ProjectBySlug(project.clone()),
            );
            push(
                format!("Agent: {name}"),
                DeepLinkTarget::AgentByName(name.clone()),
            );
        }

        MailEvent::HttpRequest { .. }
        | MailEvent::HealthPulse { .. }
        | MailEvent::ServerStarted { .. }
        | MailEvent::ServerShutdown { .. } => {
            // No entity-level correlation for infrastructure events.
        }
    }

    links
}

// ──────────────────────────────────────────────────────────────────────
// Quick actions — context-aware palette entries from focused entity
// ──────────────────────────────────────────────────────────────────────

/// A context-aware quick action derived from a focused entity.
#[derive(Debug, Clone)]
pub struct QuickAction {
    /// Unique ID for palette dispatch (e.g. "quick:agent:RedFox").
    pub id: String,
    /// Display label (e.g. "Go to Agent `RedFox`").
    pub label: String,
    /// Description for the palette.
    pub description: String,
    /// The deep-link target this action navigates to.
    pub target: DeepLinkTarget,
}

/// Build quick actions from a focused event's correlation links.
///
/// Returns actions suitable for injection into the command palette.
/// Includes both navigation ("Go to X") and macro ("Summarize thread",
/// "Fetch inbox") actions derived from the focused entity.
#[must_use]
pub fn build_quick_actions(event: &MailEvent) -> Vec<QuickAction> {
    let links = extract_links(event);
    let mut actions: Vec<QuickAction> = links
        .iter()
        .map(|link| {
            let (prefix, entity_name) = match &link.target {
                DeepLinkTarget::AgentByName(name) => ("agent", name.as_str()),
                DeepLinkTarget::ThreadById(id) => ("thread", id.as_str()),
                DeepLinkTarget::ToolByName(name) => ("tool", name.as_str()),
                DeepLinkTarget::MessageById(id) => ("message", &*format!("{id}")),
                DeepLinkTarget::ProjectBySlug(slug) => ("project", slug.as_str()),
                DeepLinkTarget::TimelineAtTime(ts) => ("timeline", &*format!("{ts}")),
                DeepLinkTarget::ReservationByAgent(agent) => ("reservations", agent.as_str()),
                DeepLinkTarget::ContactByPair(from, _to) => ("contact", from.as_str()),
                DeepLinkTarget::ExplorerForAgent(agent) => ("explorer", agent.as_str()),
                DeepLinkTarget::SearchFocused(query) => ("search", query.as_str()),
            };
            let id = format!("quick:{prefix}:{entity_name}");
            let label = format!("Go to {}", link.label);
            let description = format!("Navigate to {prefix} view");
            QuickAction {
                id,
                label,
                description,
                target: link.target.clone(),
            }
        })
        .collect();

    // Append macro actions derived from correlatable entities.
    build_macro_actions(event, &links, &mut actions);
    actions
}

/// Append macro-style quick actions (summarize, fetch inbox, etc.).
fn build_macro_actions(event: &MailEvent, links: &[CorrelationLink], out: &mut Vec<QuickAction>) {
    // Thread macros: summarize thread, view all messages in thread.
    for link in links {
        if let DeepLinkTarget::ThreadById(thread_id) = &link.target {
            out.push(QuickAction {
                id: format!("macro:summarize_thread:{thread_id}"),
                label: format!("Summarize thread {thread_id}"),
                description: "Request LLM thread summary via command palette".to_string(),
                target: DeepLinkTarget::ThreadById(thread_id.clone()),
            });
            out.push(QuickAction {
                id: format!("macro:view_thread:{thread_id}"),
                label: format!("View messages in {thread_id}"),
                description: "Open Thread Explorer focused on this thread".to_string(),
                target: DeepLinkTarget::ThreadById(thread_id.clone()),
            });
        }
    }

    // Agent macros: fetch inbox, view reservations, view in explorer.
    let mut agent_seen = std::collections::HashSet::new();
    for link in links {
        if let DeepLinkTarget::AgentByName(name) = &link.target {
            if !agent_seen.insert(name.clone()) {
                continue;
            }
            out.push(QuickAction {
                id: format!("macro:fetch_inbox:{name}"),
                label: format!("Fetch inbox for {name}"),
                description: "Open Explorer filtered to this agent's messages".to_string(),
                target: DeepLinkTarget::ExplorerForAgent(name.clone()),
            });
            out.push(QuickAction {
                id: format!("macro:view_reservations:{name}"),
                label: format!("View reservations for {name}"),
                description: "Open Reservations screen filtered to this agent".to_string(),
                target: DeepLinkTarget::ReservationByAgent(name.clone()),
            });
        }
    }

    // Tool macros: view tool call history.
    for link in links {
        if let DeepLinkTarget::ToolByName(name) = &link.target {
            out.push(QuickAction {
                id: format!("macro:tool_history:{name}"),
                label: format!("View call history for {name}"),
                description: "Open Tool Metrics screen focused on this tool".to_string(),
                target: DeepLinkTarget::ToolByName(name.clone()),
            });
        }
    }

    // Message macro: jump to message in context.
    if let MailEvent::MessageSent { id, .. } | MailEvent::MessageReceived { id, .. } = event {
        out.push(QuickAction {
            id: format!("macro:view_message:{id}"),
            label: format!("View message #{id} in context"),
            description: "Open Message Browser focused on this message".to_string(),
            target: DeepLinkTarget::MessageById(*id),
        });
    }
}

// ──────────────────────────────────────────────────────────────────────
// Remediation hints — actionable guidance for failure patterns
// ──────────────────────────────────────────────────────────────────────

/// A remediation hint with severity and suggested next action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemediationHint {
    /// Short summary of the detected issue.
    pub summary: &'static str,
    /// Suggested next action to resolve the issue.
    pub action: &'static str,
}

/// Analyze an event for common failure patterns and return remediation hints.
///
/// Returns hints for HTTP errors, slow operations, tool failures, and
/// reservation conflicts.
#[must_use]
pub fn remediation_hints(event: &MailEvent) -> Vec<RemediationHint> {
    let mut hints = Vec::new();

    match event {
        MailEvent::HttpRequest {
            status,
            duration_ms,
            ..
        } => {
            if *status == 401 || *status == 403 {
                hints.push(RemediationHint {
                    summary: "Authentication/authorization failure",
                    action: "Verify bearer token or API key in client configuration",
                });
            } else if *status == 404 {
                hints.push(RemediationHint {
                    summary: "Endpoint not found",
                    action: "Check URL path and transport mode (MCP vs API)",
                });
            } else if *status >= 500 {
                hints.push(RemediationHint {
                    summary: "Server error",
                    action: "Check server logs for stack traces; restart if persistent",
                });
            }
            if *duration_ms > 5000 {
                hints.push(RemediationHint {
                    summary: "Very slow request (>5s)",
                    action: "Check database load and query tracking metrics",
                });
            }
        }

        MailEvent::ToolCallEnd {
            result_preview,
            duration_ms,
            queries,
            ..
        } => {
            if let Some(preview) = result_preview {
                let lower = preview.to_ascii_lowercase();
                if lower.contains("error") || lower.contains("not found") {
                    hints.push(RemediationHint {
                        summary: "Tool returned an error",
                        action: "Check tool parameters and project/agent existence",
                    });
                }
            }
            if *duration_ms > 2000 {
                hints.push(RemediationHint {
                    summary: "Slow tool execution (>2s)",
                    action: "Check query count and consider adding indexes",
                });
            }
            if *queries > 50 {
                hints.push(RemediationHint {
                    summary: "Excessive queries in single tool call",
                    action: "Review query patterns; consider batching or caching",
                });
            }
        }

        MailEvent::ServerShutdown { .. } => {
            hints.push(RemediationHint {
                summary: "Server shutting down",
                action: "Restart the server with `am` to resume operations",
            });
        }

        _ => {}
    }

    hints
}

// ──────────────────────────────────────────────────────────────────────
// Inspector rendering
// ──────────────────────────────────────────────────────────────────────

/// Render an inspector detail card for the given event into `area`.
///
/// If `event` is `None`, renders an empty placeholder.
pub fn render_inspector(frame: &mut Frame<'_>, area: Rect, event: Option<&MailEvent>) {
    let Some(event) = event else {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Inspector")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let p = Paragraph::new("(select an event)").block(block);
        p.render(area, frame);
        return;
    };

    let entry = format_event(event);
    let title = format!("Inspector — {}", kind_label(event.kind()));
    let body = detail_body(event);

    // Correlation links section.
    let links = extract_links(event);
    let links_section = if links.is_empty() {
        String::new()
    } else {
        let sep = "─".repeat(area.width.saturating_sub(2) as usize);
        let mut s = format!("\n{sep}\nLinks (press 1-{}):\n", links.len().min(9));
        for (i, link) in links.iter().enumerate().take(9) {
            let _ = writeln!(s, "  [{}] {}", i + 1, link.label);
        }
        s
    };

    // Remediation hints section.
    let hints = remediation_hints(event);
    let hints_section = if hints.is_empty() {
        String::new()
    } else {
        let sep = "─".repeat(area.width.saturating_sub(2) as usize);
        let mut s = format!("\n{sep}\nHints:\n");
        for hint in &hints {
            let _ = writeln!(s, "  ⚠ {}  →  {}", hint.summary, hint.action);
        }
        s
    };

    // Combine header + body + hints + links.
    let header = format!(
        "Seq: {}  Time: {}  {}\n{}",
        event.seq(),
        entry.timestamp,
        source_label(event.source()),
        "─".repeat(area.width.saturating_sub(2) as usize),
    );
    let full_text = format!("{header}\n{body}{hints_section}{links_section}");

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let p = Paragraph::new(full_text).block(block);
    p.render(area, frame);
}

/// Resolve a 1-based link index to a deep-link target for the given event.
///
/// Returns `None` if the event has no links or the index is out of range.
#[must_use]
pub fn resolve_link(event: &MailEvent, one_based_index: usize) -> Option<DeepLinkTarget> {
    if one_based_index == 0 || one_based_index > 9 {
        return None;
    }
    let links = extract_links(event);
    links.into_iter().nth(one_based_index - 1).map(|l| l.target)
}

/// Human-readable label for the event kind.
const fn kind_label(kind: MailEventKind) -> &'static str {
    match kind {
        MailEventKind::ToolCallStart => "Tool Call (start)",
        MailEventKind::ToolCallEnd => "Tool Call (end)",
        MailEventKind::MessageSent => "Message Sent",
        MailEventKind::MessageReceived => "Message Received",
        MailEventKind::ReservationGranted => "Reservation Granted",
        MailEventKind::ReservationReleased => "Reservation Released",
        MailEventKind::AgentRegistered => "Agent Registered",
        MailEventKind::HttpRequest => "HTTP Request",
        MailEventKind::HealthPulse => "Health Pulse",
        MailEventKind::ServerStarted => "Server Started",
        MailEventKind::ServerShutdown => "Server Shutdown",
    }
}

/// Human-readable label for the event source.
const fn source_label(src: crate::tui_events::EventSource) -> &'static str {
    match src {
        crate::tui_events::EventSource::Tooling => "source:tooling",
        crate::tui_events::EventSource::Http => "source:http",
        crate::tui_events::EventSource::Mail => "source:mail",
        crate::tui_events::EventSource::Reservations => "source:reservations",
        crate::tui_events::EventSource::Lifecycle => "source:lifecycle",
        crate::tui_events::EventSource::Database => "source:database",
        crate::tui_events::EventSource::Unknown => "source:unknown",
    }
}

/// Format the event-specific detail body.
#[allow(clippy::too_many_lines)]
fn detail_body(event: &MailEvent) -> String {
    match event {
        MailEvent::ToolCallStart {
            tool_name,
            params_json,
            project,
            agent,
            redacted,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Tool: {tool_name}"));
            if let Some(p) = project {
                lines.push(format!("Project: {p}"));
            }
            if let Some(a) = agent {
                lines.push(format!("Agent: {a}"));
            }
            if *redacted {
                lines.push("⚠ Params redacted".to_string());
            } else {
                lines.push(String::new());
                lines.push("Parameters:".to_string());
                // Pretty-print JSON (already masked at event creation).
                let pretty = serde_json::to_string_pretty(params_json)
                    .unwrap_or_else(|_| params_json.to_string());
                for line in pretty.lines() {
                    lines.push(format!("  {line}"));
                }
            }
            lines.join("\n")
        }

        MailEvent::ToolCallEnd {
            tool_name,
            duration_ms,
            result_preview,
            queries,
            query_time_ms,
            per_table,
            project,
            agent,
            redacted,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Tool: {tool_name}"));
            lines.push(format!("Duration: {duration_ms}ms"));
            if let Some(p) = project {
                lines.push(format!("Project: {p}"));
            }
            if let Some(a) = agent {
                lines.push(format!("Agent: {a}"));
            }
            lines.push(format!("Queries: {queries} ({query_time_ms:.1}ms)"));
            if !per_table.is_empty() {
                lines.push("  Per table:".to_string());
                for (table, count) in per_table {
                    lines.push(format!("    {table}: {count}"));
                }
            }
            if *redacted {
                lines.push("⚠ Result redacted".to_string());
            } else if let Some(preview) = result_preview {
                lines.push(String::new());
                lines.push("Result:".to_string());
                for line in preview.lines().take(20) {
                    lines.push(format!("  {line}"));
                }
                if preview.lines().count() > 20 {
                    lines.push("  ... (truncated)".to_string());
                }
            }
            lines.join("\n")
        }

        MailEvent::MessageSent {
            id,
            from,
            to,
            subject,
            thread_id,
            project,
            ..
        }
        | MailEvent::MessageReceived {
            id,
            from,
            to,
            subject,
            thread_id,
            project,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Message ID: #{id}"));
            lines.push(format!("Project: {project}"));
            lines.push(format!("From: {from}"));
            lines.push(format!("To: {}", to.join(", ")));
            lines.push(format!("Subject: {subject}"));
            lines.push(format!("Thread: {thread_id}"));
            lines.join("\n")
        }

        MailEvent::ReservationGranted {
            agent,
            paths,
            exclusive,
            ttl_s,
            project,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Project: {project}"));
            lines.push(format!("Agent: {agent}"));
            lines.push(format!(
                "Exclusive: {}",
                if *exclusive { "yes" } else { "no" }
            ));
            lines.push(format!("TTL: {ttl_s}s"));
            lines.push(String::new());
            lines.push("Paths:".to_string());
            for path in paths {
                lines.push(format!("  {path}"));
            }
            lines.join("\n")
        }

        MailEvent::ReservationReleased {
            agent,
            paths,
            project,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Project: {project}"));
            lines.push(format!("Agent: {agent}"));
            lines.push(String::new());
            lines.push("Paths released:".to_string());
            for path in paths {
                lines.push(format!("  {path}"));
            }
            lines.join("\n")
        }

        MailEvent::AgentRegistered {
            name,
            program,
            model_name,
            project,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Agent: {name}"));
            lines.push(format!("Project: {project}"));
            lines.push(format!("Program: {program}"));
            lines.push(format!("Model: {model_name}"));
            lines.join("\n")
        }

        MailEvent::HttpRequest {
            method,
            path,
            status,
            duration_ms,
            client_ip,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("{method} {path}"));
            lines.push(format!("Status: {status}"));
            lines.push(format!("Duration: {duration_ms}ms"));
            lines.push(format!("Client: {client_ip}"));
            lines.join("\n")
        }

        MailEvent::HealthPulse { db_stats, .. } => {
            let mut lines = Vec::new();
            lines.push(format!("Projects: {}", db_stats.projects));
            lines.push(format!("Agents: {}", db_stats.agents));
            lines.push(format!("Messages: {}", db_stats.messages));
            lines.push(format!("Reservations: {}", db_stats.file_reservations));
            lines.push(format!("Contact links: {}", db_stats.contact_links));
            lines.push(format!("Ack pending: {}", db_stats.ack_pending));
            if !db_stats.agents_list.is_empty() {
                lines.push(String::new());
                lines.push("Active agents:".to_string());
                for a in &db_stats.agents_list {
                    lines.push(format!("  {} ({})", a.name, a.program));
                }
            }
            lines.join("\n")
        }

        MailEvent::ServerStarted {
            endpoint,
            config_summary,
            ..
        } => {
            let mut lines = Vec::new();
            lines.push(format!("Endpoint: {endpoint}"));
            if !config_summary.is_empty() {
                lines.push(String::new());
                lines.push("Config:".to_string());
                for line in config_summary.lines() {
                    lines.push(format!("  {line}"));
                }
            }
            lines.join("\n")
        }

        MailEvent::ServerShutdown { .. } => "Server is shutting down.".to_string(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::{DbStatSnapshot, EventSource};
    use serde_json::json;

    #[test]
    fn render_inspector_no_event() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 60, 20), None);
    }

    #[test]
    fn render_inspector_tool_call_start() {
        let event = MailEvent::tool_call_start(
            "send_message",
            json!({"project_key": "test", "body": "hello"}),
            Some("my-project".to_string()),
            Some("RedFox".to_string()),
        );
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 30, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 30), Some(&event));
    }

    #[test]
    fn render_inspector_tool_call_end() {
        let event = MailEvent::tool_call_end(
            "send_message",
            42,
            Some("ok".to_string()),
            3,
            1.5,
            vec![("messages".to_string(), 2), ("projects".to_string(), 1)],
            Some("my-project".to_string()),
            Some("RedFox".to_string()),
        );
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 30, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 30), Some(&event));
    }

    #[test]
    fn render_inspector_message_sent() {
        let event = MailEvent::message_sent(
            42,
            "RedFox",
            vec!["BlueLake".to_string()],
            "Hello",
            "thread-1",
            "my-project",
        );
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 20, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 20), Some(&event));
    }

    #[test]
    fn render_inspector_http_request() {
        let event = MailEvent::http_request("GET", "/mcp/", 200, 15, "10.0.0.1");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 15, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 60, 15), Some(&event));
    }

    #[test]
    fn render_inspector_reservation_granted() {
        let event = MailEvent::reservation_granted(
            "RedFox",
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            true,
            3600,
            "my-project",
        );
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 20, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 20), Some(&event));
    }

    #[test]
    fn render_inspector_agent_registered() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "my-project");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 15, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 60, 15), Some(&event));
    }

    #[test]
    fn render_inspector_health_pulse() {
        let event = MailEvent::health_pulse(DbStatSnapshot {
            projects: 3,
            agents: 5,
            messages: 100,
            file_reservations: 10,
            contact_links: 2,
            ack_pending: 1,
            agents_list: vec![],
            timestamp_micros: 0,
            ..Default::default()
        });
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 60, 20), Some(&event));
    }

    #[test]
    fn render_inspector_server_started() {
        let event = MailEvent::server_started("http://127.0.0.1:8765/mcp/", "db=mail.db pool=5");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 15, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 15), Some(&event));
    }

    #[test]
    fn render_inspector_server_shutdown() {
        let event = MailEvent::server_shutdown();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 10, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 60, 10), Some(&event));
    }

    #[test]
    fn kind_label_all_variants() {
        assert_eq!(
            kind_label(MailEventKind::ToolCallStart),
            "Tool Call (start)"
        );
        assert_eq!(kind_label(MailEventKind::ToolCallEnd), "Tool Call (end)");
        assert_eq!(kind_label(MailEventKind::MessageSent), "Message Sent");
        assert_eq!(kind_label(MailEventKind::HttpRequest), "HTTP Request");
        assert_eq!(kind_label(MailEventKind::ServerStarted), "Server Started");
        assert_eq!(kind_label(MailEventKind::ServerShutdown), "Server Shutdown");
    }

    #[test]
    fn source_label_all_variants() {
        assert_eq!(source_label(EventSource::Tooling), "source:tooling");
        assert_eq!(source_label(EventSource::Http), "source:http");
        assert_eq!(source_label(EventSource::Mail), "source:mail");
    }

    #[test]
    fn detail_body_tool_call_start_shows_params() {
        let event = MailEvent::tool_call_start("send_message", json!({"key": "value"}), None, None);
        let body = detail_body(&event);
        assert!(body.contains("Tool: send_message"));
        assert!(body.contains("Parameters:"));
        assert!(body.contains("\"key\""));
    }

    #[test]
    fn detail_body_tool_call_end_shows_per_table() {
        let event = MailEvent::tool_call_end(
            "fetch_inbox",
            100,
            Some("3 messages".to_string()),
            5,
            2.5,
            vec![("messages".to_string(), 3), ("projects".to_string(), 2)],
            None,
            None,
        );
        let body = detail_body(&event);
        assert!(body.contains("Duration: 100ms"));
        assert!(body.contains("messages: 3"));
        assert!(body.contains("Result:"));
    }

    #[test]
    fn detail_body_http_request_shows_all_fields() {
        let event = MailEvent::http_request("POST", "/mcp/", 201, 42, "192.168.1.1");
        let body = detail_body(&event);
        assert!(body.contains("POST /mcp/"));
        assert!(body.contains("Status: 201"));
        assert!(body.contains("Duration: 42ms"));
        assert!(body.contains("Client: 192.168.1.1"));
    }

    #[test]
    fn detail_body_reservation_shows_paths() {
        let event = MailEvent::reservation_granted(
            "RedFox",
            vec!["a.rs".to_string(), "b.rs".to_string()],
            false,
            600,
            "proj",
        );
        let body = detail_body(&event);
        assert!(body.contains("a.rs"));
        assert!(body.contains("b.rs"));
        assert!(body.contains("Exclusive: no"));
        assert!(body.contains("TTL: 600s"));
    }

    #[test]
    fn detail_body_redacted_tool_call() {
        let mut event =
            MailEvent::tool_call_start("send_message", json!({"secret": "value"}), None, None);
        // Simulate redaction flag.
        if let MailEvent::ToolCallStart { redacted, .. } = &mut event {
            *redacted = true;
        }
        let body = detail_body(&event);
        assert!(body.contains("⚠ Params redacted"));
        assert!(!body.contains("Parameters:"));
    }

    #[test]
    fn render_inspector_minimum_size() {
        let event = MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(30, 5, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 30, 5), Some(&event));
    }

    // ── Correlation link tests ────────────────────────────────────

    #[test]
    fn extract_links_tool_call_start() {
        let event = MailEvent::tool_call_start(
            "send_message",
            json!({}),
            Some("my-proj".to_string()),
            Some("RedFox".to_string()),
        );
        let links = extract_links(&event);
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("my-proj".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("RedFox".into())
        );
        assert_eq!(
            links[2].target,
            DeepLinkTarget::ToolByName("send_message".into())
        );
    }

    #[test]
    fn extract_links_tool_call_start_no_project_no_agent() {
        let event = MailEvent::tool_call_start("health_check", json!({}), None, None);
        let links = extract_links(&event);
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ToolByName("health_check".into())
        );
    }

    #[test]
    fn extract_links_tool_call_end() {
        let event = MailEvent::tool_call_end(
            "fetch_inbox",
            50,
            None,
            2,
            1.0,
            vec![],
            Some("proj".to_string()),
            Some("BlueLake".to_string()),
        );
        let links = extract_links(&event);
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("proj".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("BlueLake".into())
        );
        assert_eq!(
            links[2].target,
            DeepLinkTarget::ToolByName("fetch_inbox".into())
        );
    }

    #[test]
    fn extract_links_message_sent() {
        let event = MailEvent::message_sent(
            42,
            "RedFox",
            vec!["BlueLake".to_string(), "GoldHawk".to_string()],
            "Hello",
            "thread-1",
            "my-project",
        );
        let links = extract_links(&event);
        // project, from-agent, to-agent1, to-agent2, thread, message
        assert_eq!(links.len(), 6);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("my-project".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("RedFox".into())
        );
        assert_eq!(
            links[2].target,
            DeepLinkTarget::AgentByName("BlueLake".into())
        );
        assert_eq!(
            links[3].target,
            DeepLinkTarget::AgentByName("GoldHawk".into())
        );
        assert_eq!(
            links[4].target,
            DeepLinkTarget::ThreadById("thread-1".into())
        );
        assert_eq!(links[5].target, DeepLinkTarget::MessageById(42));
    }

    #[test]
    fn extract_links_message_deduplicates() {
        // Sender is also in recipients — should not produce duplicate
        let event = MailEvent::message_sent(
            10,
            "RedFox",
            vec!["RedFox".to_string()],
            "Self-message",
            "t",
            "p",
        );
        let links = extract_links(&event);
        let agent_links = links
            .iter()
            .filter(|l| matches!(&l.target, DeepLinkTarget::AgentByName(n) if n == "RedFox"))
            .count();
        assert_eq!(agent_links, 1, "duplicate agent links should be suppressed");
    }

    #[test]
    fn extract_links_reservation_granted() {
        let event = MailEvent::reservation_granted(
            "RedFox",
            vec!["src/lib.rs".to_string()],
            true,
            3600,
            "my-proj",
        );
        let links = extract_links(&event);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("my-proj".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("RedFox".into())
        );
    }

    #[test]
    fn extract_links_reservation_released() {
        let event = MailEvent::reservation_released("BlueLake", vec!["a.rs".to_string()], "proj");
        let links = extract_links(&event);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("proj".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("BlueLake".into())
        );
    }

    #[test]
    fn extract_links_agent_registered() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "proj");
        let links = extract_links(&event);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0].target,
            DeepLinkTarget::ProjectBySlug("proj".into())
        );
        assert_eq!(
            links[1].target,
            DeepLinkTarget::AgentByName("RedFox".into())
        );
    }

    #[test]
    fn extract_links_http_request_is_empty() {
        let event = MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1");
        assert!(extract_links(&event).is_empty());
    }

    #[test]
    fn extract_links_health_pulse_is_empty() {
        let event = MailEvent::health_pulse(DbStatSnapshot::default());
        assert!(extract_links(&event).is_empty());
    }

    #[test]
    fn extract_links_server_events_are_empty() {
        assert!(extract_links(&MailEvent::server_started("http://localhost", "")).is_empty());
        assert!(extract_links(&MailEvent::server_shutdown()).is_empty());
    }

    #[test]
    fn resolve_link_valid_index() {
        let event = MailEvent::tool_call_start(
            "send_message",
            json!({}),
            Some("proj".to_string()),
            Some("RedFox".to_string()),
        );
        assert_eq!(
            resolve_link(&event, 1),
            Some(DeepLinkTarget::ProjectBySlug("proj".into()))
        );
        assert_eq!(
            resolve_link(&event, 2),
            Some(DeepLinkTarget::AgentByName("RedFox".into()))
        );
        assert_eq!(
            resolve_link(&event, 3),
            Some(DeepLinkTarget::ToolByName("send_message".into()))
        );
    }

    #[test]
    fn resolve_link_out_of_range() {
        let event = MailEvent::tool_call_start("x", json!({}), None, None);
        // Only 1 link (tool name)
        assert!(resolve_link(&event, 0).is_none());
        assert!(resolve_link(&event, 2).is_none());
        assert!(resolve_link(&event, 10).is_none());
    }

    #[test]
    fn resolve_link_no_links() {
        let event = MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1");
        assert!(resolve_link(&event, 1).is_none());
    }

    #[test]
    fn render_inspector_with_links_shows_link_section() {
        let event = MailEvent::message_sent(
            1,
            "RedFox",
            vec!["BlueLake".to_string()],
            "Hello",
            "thread-1",
            "proj",
        );
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 40, &mut pool);
        render_inspector(&mut frame, Rect::new(0, 0, 80, 40), Some(&event));
        // If the frame renders without panic, the link section was included
    }

    #[test]
    fn correlation_link_label_format() {
        let event = MailEvent::agent_registered("GoldHawk", "codex", "5.2", "p");
        let links = extract_links(&event);
        assert!(links[0].label.contains("Project: p"));
        assert!(links[1].label.contains("Agent: GoldHawk"));
    }

    // ── Quick action tests ──────────────────────────────────────

    #[test]
    fn quick_actions_from_tool_call() {
        let event = MailEvent::tool_call_start(
            "send_message",
            serde_json::Value::Null,
            Some("my_project".to_string()),
            Some("RedFox".to_string()),
        );
        let actions = build_quick_actions(&event);
        assert!(!actions.is_empty());

        // Should have project, agent, and tool actions
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.starts_with("quick:project:")));
        assert!(ids.iter().any(|id| id.starts_with("quick:agent:")));
        assert!(ids.iter().any(|id| id.starts_with("quick:tool:")));
    }

    #[test]
    fn quick_actions_from_agent_registered() {
        let event = MailEvent::agent_registered("GoldHawk", "codex", "5.2", "proj1");
        let actions = build_quick_actions(&event);
        // 2 navigation + 2 agent macros (fetch_inbox, view_reservations)
        assert_eq!(actions.len(), 4);

        // First two are navigation
        assert!(actions[0].id.contains("project:"));
        assert!(actions[0].label.contains("Go to"));
        assert!(actions[1].id.contains("agent:GoldHawk"));
        // Last two are macros
        assert!(actions[2].id.starts_with("macro:"));
        assert!(actions[3].id.starts_with("macro:"));
    }

    #[test]
    fn quick_actions_empty_for_infra_events() {
        let event = MailEvent::server_started("http", "127.0.0.1:8080");
        let actions = build_quick_actions(&event);
        assert!(actions.is_empty());
    }

    #[test]
    fn quick_actions_labels_are_well_formed() {
        let event = MailEvent::agent_registered("GoldHawk", "codex", "5.2", "proj1");
        let actions = build_quick_actions(&event);
        for action in &actions {
            // Navigation actions start with "Go to", macro actions have descriptive labels
            assert!(
                action.label.starts_with("Go to ") || action.id.starts_with("macro:"),
                "unexpected label: {} (id: {})",
                action.label,
                action.id
            );
            assert!(!action.id.is_empty());
            assert!(!action.description.is_empty());
        }
    }

    #[test]
    fn quick_actions_deduplicates() {
        // Message events can have sender == recipient; ensure dedup
        let event = MailEvent::message_sent(
            1,
            "RedFox",
            vec!["RedFox".to_string()],
            "Hi",
            "thread1",
            "proj1",
        );
        let actions = build_quick_actions(&event);
        let agent_action_count = actions
            .iter()
            .filter(|a| a.id.starts_with("quick:agent:"))
            .count();
        // Should only have one RedFox entry (deduped by extract_links)
        assert_eq!(agent_action_count, 1);
    }

    // ── Remediation hints tests ─────────────────────────────────

    #[test]
    fn hints_http_401() {
        let event = MailEvent::http_request("POST", "/mcp/", 401, 5, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].summary.contains("Authentication"));
    }

    #[test]
    fn hints_http_403() {
        let event = MailEvent::http_request("GET", "/api/", 403, 2, "10.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].action.contains("token"));
    }

    #[test]
    fn hints_http_404() {
        let event = MailEvent::http_request("GET", "/bad", 404, 1, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].summary.contains("not found"));
    }

    #[test]
    fn hints_http_500() {
        let event = MailEvent::http_request("POST", "/mcp/", 500, 10, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].summary.contains("Server error"));
    }

    #[test]
    fn hints_http_slow() {
        let event = MailEvent::http_request("POST", "/mcp/", 200, 6000, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].summary.contains("slow"));
    }

    #[test]
    fn hints_http_500_and_slow() {
        let event = MailEvent::http_request("POST", "/mcp/", 500, 6000, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 2);
    }

    #[test]
    fn hints_http_200_fast_no_hints() {
        let event = MailEvent::http_request("GET", "/mcp/", 200, 5, "127.0.0.1");
        let hints = remediation_hints(&event);
        assert!(hints.is_empty());
    }

    #[test]
    fn hints_tool_error_in_preview() {
        let event = MailEvent::tool_call_end(
            "send_message",
            100,
            Some("Error: agent not found".to_string()),
            5,
            1.2,
            vec![],
            None,
            None,
        );
        let hints = remediation_hints(&event);
        assert!(!hints.is_empty());
        assert!(hints[0].summary.contains("error"));
    }

    #[test]
    fn hints_tool_slow() {
        let event =
            MailEvent::tool_call_end("fetch_inbox", 3000, None, 10, 50.0, vec![], None, None);
        let hints = remediation_hints(&event);
        assert!(!hints.is_empty());
        assert!(hints[0].summary.contains("Slow"));
    }

    #[test]
    fn hints_tool_excessive_queries() {
        let event =
            MailEvent::tool_call_end("search_messages", 500, None, 60, 120.0, vec![], None, None);
        let hints = remediation_hints(&event);
        assert!(!hints.is_empty());
        assert!(hints.iter().any(|h| h.summary.contains("Excessive")));
    }

    #[test]
    fn hints_tool_ok_no_hints() {
        let event = MailEvent::tool_call_end(
            "send_message",
            50,
            Some("ok".to_string()),
            3,
            1.0,
            vec![],
            None,
            None,
        );
        let hints = remediation_hints(&event);
        assert!(hints.is_empty());
    }

    #[test]
    fn hints_server_shutdown() {
        let event = MailEvent::server_shutdown();
        let hints = remediation_hints(&event);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].action.contains("am"));
    }

    #[test]
    fn hints_message_sent_no_hints() {
        let event = MailEvent::message_sent(
            1,
            "RedFox",
            vec!["BlueFox".to_string()],
            "Test",
            "thread1",
            "proj1",
        );
        let hints = remediation_hints(&event);
        assert!(hints.is_empty());
    }

    // ── Macro quick action tests ──────────────────────────────────

    #[test]
    fn macro_actions_for_message_include_thread_and_agent() {
        let event = MailEvent::message_sent(
            42,
            "RedFox",
            vec!["BlueLake".to_string()],
            "Hello",
            "thread-1",
            "my-project",
        );
        let actions = build_quick_actions(&event);
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();

        // Should have macro actions for thread, agents
        assert!(
            ids.contains(&"macro:summarize_thread:thread-1"),
            "missing summarize thread macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:view_thread:thread-1"),
            "missing view thread macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:fetch_inbox:RedFox"),
            "missing fetch inbox macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:view_reservations:RedFox"),
            "missing view reservations macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:view_message:42"),
            "missing view message macro: {ids:?}"
        );
    }

    #[test]
    fn macro_actions_for_tool_include_tool_history() {
        let event = MailEvent::tool_call_start(
            "send_message",
            serde_json::Value::Null,
            Some("proj".to_string()),
            Some("RedFox".to_string()),
        );
        let actions = build_quick_actions(&event);
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();

        assert!(
            ids.contains(&"macro:tool_history:send_message"),
            "missing tool history macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:fetch_inbox:RedFox"),
            "missing fetch inbox macro for tool agent: {ids:?}"
        );
    }

    #[test]
    fn macro_actions_agent_dedup() {
        // Message with sender==recipient: agent macros should not duplicate.
        let event =
            MailEvent::message_sent(1, "RedFox", vec!["RedFox".to_string()], "Self", "t", "p");
        let actions = build_quick_actions(&event);
        let fetch_inbox_count = actions
            .iter()
            .filter(|a| a.id.starts_with("macro:fetch_inbox:"))
            .count();
        assert_eq!(fetch_inbox_count, 1, "agent macros should be deduped");
    }

    #[test]
    fn macro_actions_empty_for_infra_events() {
        let event = MailEvent::server_started("http://localhost", "config");
        let actions = build_quick_actions(&event);
        let macro_count = actions
            .iter()
            .filter(|a| a.id.starts_with("macro:"))
            .count();
        assert_eq!(macro_count, 0, "no macros for infrastructure events");
    }

    #[test]
    fn macro_actions_reservation_includes_agent_macros() {
        let event = MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/lib.rs".to_string()],
            true,
            3600,
            "my-proj",
        );
        let actions = build_quick_actions(&event);
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();

        assert!(
            ids.contains(&"macro:fetch_inbox:BlueLake"),
            "reservation agent should get fetch_inbox macro: {ids:?}"
        );
        assert!(
            ids.contains(&"macro:view_reservations:BlueLake"),
            "reservation agent should get view_reservations macro: {ids:?}"
        );
    }
}
