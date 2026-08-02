//! Portable screen contract and public-replay screens used by the browser shell.

use std::cell::Cell;

use ftui::Event;
use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, Style};
use ftui_runtime::program::Cmd;

use crate::state::TuiSharedState;
use crate::tui_events::MailEvent;

pub use crate::tui_screen_registry::*;

#[derive(Debug, Clone)]
pub struct HelpEntry {
    pub key: &'static str,
    pub action: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataGeneration {
    pub event_total_pushed: u64,
    pub console_log_seq: u64,
    pub db_stats_gen: u64,
    pub request_gen: u64,
}

impl DataGeneration {
    #[must_use]
    pub const fn stale() -> Self {
        Self {
            event_total_pushed: u64::MAX,
            console_log_seq: u64::MAX,
            db_stats_gen: u64::MAX,
            request_gen: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct DirtyFlags {
    pub events: bool,
    pub console_log: bool,
    pub db_stats: bool,
    pub requests: bool,
}

impl DirtyFlags {
    #[must_use]
    pub const fn any(self) -> bool {
        self.events || self.console_log || self.db_stats || self.requests
    }
}

#[must_use]
pub const fn dirty_since(previous: &DataGeneration, current: &DataGeneration) -> DirtyFlags {
    DirtyFlags {
        events: current.event_total_pushed != previous.event_total_pushed,
        console_log: current.console_log_seq != previous.console_log_seq,
        db_stats: current.db_stats_gen != previous.db_stats_gen,
        requests: current.request_gen != previous.request_gen,
    }
}

#[derive(Debug, Clone)]
pub enum MailScreenMsg {
    Noop,
    DeepLink(DeepLinkTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLinkTarget {
    TimelineAtTime(i64),
    SearchFocused(String),
}

pub trait MailScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg>;
    fn view(&self, frame: &mut ftui::Frame<'_>, area: Rect, state: &TuiSharedState);
    fn tick(&mut self, _tick_count: u64, _state: &TuiSharedState) {}
    fn prefers_fast_tick(&self, _state: &TuiSharedState) -> bool {
        false
    }
    fn keybindings(&self) -> Vec<HelpEntry> {
        Vec::new()
    }
    fn context_help_tip(&self) -> Option<&'static str> {
        None
    }
    fn consumes_text_input(&self) -> bool {
        false
    }
    fn title(&self) -> &'static str {
        "Dashboard"
    }
    fn tab_label(&self) -> &'static str {
        self.title()
    }
}

#[must_use]
pub fn contains_ci(text: &str, query: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

/// Browser-safe read-only screen used for registry entries whose native
/// implementations depend on the database, filesystem, or server runtime.
/// Every row is derived from the validated public replay pack, and mouse/keys
/// operate on the same rendered list rectangle cached during `view()`.
pub struct PublicReplayScreen {
    list_area: Cell<Rect>,
    selected: usize,
    scroll: usize,
    search_query: String,
    search_active: bool,
}

impl Default for PublicReplayScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicReplayScreen {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            list_area: Cell::new(Rect::new(0, 0, 0, 0)),
            selected: 0,
            scroll: 0,
            search_query: String::new(),
            search_active: false,
        }
    }

    pub fn reset(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.search_query.clear();
        self.search_active = false;
        self.list_area.set(Rect::new(0, 0, 0, 0));
    }

    pub fn begin_search(&mut self, query: &str) {
        self.search_query = query.chars().take(96).collect();
        self.search_active = true;
        self.selected = 0;
        self.scroll = 0;
    }

    #[must_use]
    pub fn consumes_text_input(&self, screen: MailScreenId) -> bool {
        screen == MailScreenId::Search && self.search_active
    }

    #[must_use]
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    #[must_use]
    pub const fn selected_row(&self) -> usize {
        self.selected
    }

    /// Reconcile preserved selection/scroll state with freshly reset replay
    /// data so a shorter result set cannot leave the list scrolled past EOF.
    pub fn normalize(&mut self, screen: MailScreenId, state: &TuiSharedState) {
        let row_count = self.visible_rows(screen, state).len();
        let page = usize::from(self.list_area.get().height.max(1));
        self.clamp_to_rows(row_count, page);
    }

    fn clamp_to_rows(&mut self, row_count: usize, page: usize) {
        if row_count == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(row_count - 1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(page) {
            self.scroll = self.selected + 1 - page;
        }
        self.scroll = self.scroll.min(row_count.saturating_sub(1));
    }

    /// Process list navigation. Returns true when visible selection or scroll
    /// state changed, allowing the host status contract to expose interaction.
    pub fn update(&mut self, event: &Event, screen: MailScreenId, state: &TuiSharedState) -> bool {
        let previous = (
            self.selected,
            self.scroll,
            self.search_query.clone(),
            self.search_active,
        );
        let page = usize::from(self.list_area.get().height.max(1));
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && screen == MailScreenId::Search
                    && self.search_active =>
            {
                match key.code {
                    KeyCode::Escape | KeyCode::Enter => self.search_active = false,
                    KeyCode::Backspace => {
                        self.search_query.pop();
                        self.selected = 0;
                        self.scroll = 0;
                    }
                    KeyCode::Char(character)
                        if !character.is_control()
                            && !key.modifiers.intersects(
                                Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER,
                            ) =>
                    {
                        if self.search_query.chars().count() < 96 {
                            self.search_query.push(character);
                            self.selected = 0;
                            self.scroll = 0;
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(paste) if screen == MailScreenId::Search && self.search_active => {
                let remaining = 96_usize.saturating_sub(self.search_query.chars().count());
                self.search_query.extend(paste.text.chars().take(remaining));
                self.selected = 0;
                self.scroll = 0;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = self.selected.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = self.selected.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.selected = self.selected.saturating_add(page);
                }
                KeyCode::PageUp => {
                    self.selected = self.selected.saturating_sub(page);
                }
                KeyCode::Home => self.selected = 0,
                KeyCode::End => self.selected = usize::MAX,
                _ => return false,
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    let relative = usize::from(mouse.y.saturating_sub(self.list_area.get().y));
                    self.selected = self.scroll.saturating_add(relative);
                }
                MouseEventKind::ScrollDown
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    self.selected = self.selected.saturating_add(3);
                }
                MouseEventKind::ScrollUp
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    self.selected = self.selected.saturating_sub(3);
                }
                _ => return false,
            },
            _ => return false,
        }
        let row_count = self.visible_rows(screen, state).len();
        self.clamp_to_rows(row_count, page);
        (
            self.selected,
            self.scroll,
            self.search_query.clone(),
            self.search_active,
        ) != previous
    }

    fn visible_rows(&self, screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
        let mut rows = public_rows(screen, state);
        if screen == MailScreenId::Search && !self.search_query.is_empty() {
            rows.retain(|row| contains_ci(row, &self.search_query));
        }
        rows
    }

    pub fn view(
        &self,
        screen: MailScreenId,
        frame: &mut ftui::Frame<'_>,
        area: Rect,
        state: &TuiSharedState,
    ) {
        let palette = crate::tui_theme::TuiThemePalette::current();
        let meta = screen_meta(screen);
        let rows = self.visible_rows(screen, state);

        let header_height = area.height.min(3);
        let header_area = Rect::new(area.x, area.y, area.width, header_height);
        let body_area = Rect::new(
            area.x,
            area.y.saturating_add(header_height),
            area.width,
            area.height.saturating_sub(header_height),
        );
        Paragraph::new(Text::from_lines([
            Line::from_spans([
                Span::styled(
                    format!(" {} ", meta.title),
                    Style::default().fg(palette.status_accent).bold(),
                ),
                Span::styled(
                    "PUBLIC REPLAY · READ-ONLY",
                    Style::default().fg(palette.severity_ok).bold(),
                ),
            ]),
            Line::from_spans([Span::styled(
                meta.description,
                Style::default().fg(palette.text_secondary),
            )]),
            Line::from_spans([Span::styled(
                if screen == MailScreenId::Search {
                    format!(
                        "Live Search: {}{}",
                        if self.search_query.is_empty() {
                            "type to filter events".to_string()
                        } else {
                            self.search_query.clone()
                        },
                        if self.search_active { " ▌" } else { " · press / to edit" }
                    )
                } else {
                    "Click rows or use ↑/↓, j/k, PageUp/PageDown · click any top tab to switch screens".to_string()
                },
                Style::default().fg(palette.text_muted),
            )]),
        ]))
        .style(Style::default().bg(palette.bg_deep))
        .render(header_area, frame);

        if body_area.height == 0 {
            self.list_area.set(Rect::new(0, 0, 0, 0));
            return;
        }
        let title = format!(" {} records · sanitized public demo details ", rows.len());
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(&title)
            .style(
                Style::default()
                    .fg(palette.panel_border)
                    .bg(palette.panel_bg),
            );
        let inner = block.inner(body_area);
        block.render(body_area, frame);
        self.list_area.set(inner);

        let lines = rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(usize::from(inner.height))
            .map(|(index, row)| {
                let selected = index == self.selected;
                let indicator = if selected { "▶" } else { " " };
                let style = if selected {
                    Style::default()
                        .fg(palette.selection_fg)
                        .bg(palette.selection_bg)
                        .bold()
                } else if index % 2 == 1 {
                    Style::default()
                        .fg(palette.text_primary)
                        .bg(palette.table_row_alt_bg)
                } else {
                    Style::default()
                        .fg(palette.text_primary)
                        .bg(palette.panel_bg)
                };
                Line::from_spans([Span::styled(format!("{indicator} {row}"), style)])
            })
            .collect::<Vec<_>>();
        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }
}

fn public_rows(screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
    let stats = state.db_stats_snapshot().unwrap_or_default();
    match screen {
        MailScreenId::Agents => stats
            .agents_list
            .iter()
            .map(|agent| {
                format!(
                    "● {:<20} {:<14} {:<16} {}",
                    agent.name, agent.program, agent.model, agent.project
                )
            })
            .collect(),
        MailScreenId::Projects => stats
            .projects_list
            .iter()
            .map(|project| {
                format!(
                    "▣ {:<26} agents:{:<4} messages:{:<7} locks:{:<4}",
                    project.slug,
                    project.agent_count,
                    project.message_count,
                    project.reservation_count
                )
            })
            .collect(),
        MailScreenId::Contacts => stats
            .contacts_list
            .iter()
            .map(|contact| {
                format!(
                    "◉ {:<18} → {:<18} {:<10} {}",
                    contact.from_agent, contact.to_agent, contact.status, contact.reason
                )
            })
            .collect(),
        MailScreenId::Reservations | MailScreenId::ArchiveBrowser => stats
            .reservation_snapshots
            .iter()
            .map(|reservation| {
                format!(
                    "{} {:<18} {:<26} {}",
                    if reservation.exclusive { "X" } else { "S" },
                    reservation.agent_name,
                    reservation.project_slug,
                    reservation.path_pattern
                )
            })
            .collect(),
        MailScreenId::SystemHealth => vec![
            format!(
                "Database snapshot     projects:{:<5} agents:{:<6} messages:{}",
                stats.projects, stats.agents, stats.messages
            ),
            format!("Reservation ledger    active:{}", stats.file_reservations),
            format!("Contact graph         links:{}", stats.contact_links),
            format!("Acknowledgements      pending:{}", stats.ack_pending),
            format!("Transport             {}", state.transport_mode_label()),
            format!("Request latency       avg:{}ms", state.avg_latency_ms()),
        ],
        MailScreenId::Atc => stats
            .agents_list
            .iter()
            .take(200)
            .enumerate()
            .map(|(index, agent)| {
                format!(
                    "{} {:<20} {:<20} evidence:{:03} conflicts:{}",
                    if index % 7 == 0 { "HOLD" } else { "CLEAR" },
                    agent.name,
                    agent.project,
                    90 + index % 10,
                    usize::from(index % 11 == 0)
                )
            })
            .collect(),
        _ => state
            .tick_events_since_limited(0, 2_000)
            .into_iter()
            .filter(|event| event_visible_on(screen, event))
            .map(|event| public_event_row(&event))
            .collect(),
    }
}

fn event_visible_on(screen: MailScreenId, event: &MailEvent) -> bool {
    match screen {
        MailScreenId::Messages | MailScreenId::Threads | MailScreenId::Attachments => matches!(
            event,
            MailEvent::MessageSent { .. } | MailEvent::MessageReceived { .. }
        ),
        MailScreenId::ToolMetrics | MailScreenId::Analytics => matches!(
            event,
            MailEvent::ToolCallStart { .. }
                | MailEvent::ToolCallEnd { .. }
                | MailEvent::HttpRequest { .. }
                | MailEvent::GitSegfaultRetry { .. }
        ),
        MailScreenId::Dashboard
        | MailScreenId::Agents
        | MailScreenId::Reservations
        | MailScreenId::SystemHealth
        | MailScreenId::Projects
        | MailScreenId::Contacts
        | MailScreenId::ArchiveBrowser
        | MailScreenId::Atc => false,
        MailScreenId::Search | MailScreenId::Timeline | MailScreenId::Explorer => true,
    }
}

fn public_event_row(event: &MailEvent) -> String {
    let prefix = format!("#{:<5} {:<18}", event.seq(), format!("{:?}", event.kind()));
    let detail = match event {
        MailEvent::MessageSent {
            from,
            to,
            subject,
            project,
            ..
        }
        | MailEvent::MessageReceived {
            from,
            to,
            subject,
            project,
            ..
        } => format!("{from} → {} · {subject} · {project}", to.join(",")),
        MailEvent::ReservationGranted {
            agent,
            paths,
            project,
            ..
        }
        | MailEvent::ReservationReleased {
            agent,
            paths,
            project,
            ..
        } => format!("{agent} · {} · {project}", paths.join(",")),
        MailEvent::AgentRegistered {
            name,
            program,
            model_name,
            project,
            ..
        } => format!("{name} · {program}/{model_name} · {project}"),
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
        } => format!(
            "{tool_name} · {} · {}",
            agent.as_deref().unwrap_or("system"),
            project.as_deref().unwrap_or("global")
        ),
        MailEvent::HttpRequest {
            method,
            path,
            status,
            duration_ms,
            ..
        } => format!("{method} {path} · {status} · {duration_ms}ms"),
        MailEvent::HealthPulse { db_stats, .. } => format!(
            "projects:{} agents:{} messages:{}",
            db_stats.projects, db_stats.agents, db_stats.messages
        ),
        MailEvent::GitSegfaultRetry {
            name,
            repo_slug,
            attempt_n,
            ..
        } => format!("{name} · {repo_slug} · attempt {attempt_n}"),
        MailEvent::ServerStarted { endpoint, .. } => endpoint.clone(),
        MailEvent::ServerShutdown { .. } => "server stopped".to_string(),
    };
    format!("{prefix} {detail}")
}

#[path = "../../mcp-agent-mail-server/src/tui_screens/dashboard.rs"]
pub mod dashboard;
