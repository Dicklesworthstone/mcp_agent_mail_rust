//! Portable screen contract and public-replay screens used by the browser shell.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

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
    row_cache: RefCell<Option<PublicRowsCache>>,
}

#[derive(Debug, Clone)]
struct PublicRowsCache {
    screen: MailScreenId,
    generation: DataGeneration,
    query: String,
    rows: Vec<String>,
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
            row_cache: RefCell::new(None),
        }
    }

    pub fn reset(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.search_query.clear();
        self.search_active = false;
        self.list_area.set(Rect::new(0, 0, 0, 0));
        *self.row_cache.get_mut() = None;
    }

    pub fn begin_search(&mut self, query: &str) {
        self.search_query = query.chars().take(96).collect();
        self.search_active = true;
        self.selected = 0;
        self.scroll = 0;
        *self.row_cache.get_mut() = None;
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

    /// Focus the public timeline row whose event timestamp is closest to the
    /// production Dashboard deep-link target.
    pub fn focus_timeline_at(&mut self, timestamp_micros: i64, state: &TuiSharedState) -> bool {
        let events = state.tick_events_since_limited(0, 2_000);
        let Some(index) = events
            .iter()
            .filter(|event| event_visible_on(MailScreenId::Timeline, event))
            .enumerate()
            .min_by_key(|(_, event)| event.timestamp_micros().abs_diff(timestamp_micros))
            .map(|(index, _)| index)
        else {
            return false;
        };
        let previous = (self.selected, self.scroll);
        self.selected = index;
        let page = usize::from(self.list_area.get().height.max(1));
        self.scroll = self.selected.saturating_sub(page / 2);
        previous != (self.selected, self.scroll)
    }

    fn ensure_rows(&self, screen: MailScreenId, state: &TuiSharedState) {
        let generation = state.data_generation();
        let needs_rebuild = self.row_cache.borrow().as_ref().is_none_or(|cache| {
            cache.screen != screen
                || cache.generation != generation
                || cache.query != self.search_query
        });
        if !needs_rebuild {
            return;
        }
        let mut rows = public_rows(screen, state);
        if screen == MailScreenId::Search && !self.search_query.is_empty() {
            let normalized_query = self.search_query.to_ascii_lowercase();
            rows.retain(|row| row.to_ascii_lowercase().contains(&normalized_query));
        }
        *self.row_cache.borrow_mut() = Some(PublicRowsCache {
            screen,
            generation,
            query: self.search_query.clone(),
            rows,
        });
    }

    fn visible_row_count(&self, screen: MailScreenId, state: &TuiSharedState) -> usize {
        self.ensure_rows(screen, state);
        self.row_cache
            .borrow()
            .as_ref()
            .map_or(0, |cache| cache.rows.len())
    }

    /// Reconcile preserved selection/scroll state with freshly reset replay
    /// data so a shorter result set cannot leave the list scrolled past EOF.
    pub fn normalize(&mut self, screen: MailScreenId, state: &TuiSharedState) {
        let row_count = self.visible_row_count(screen, state);
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
                    let remaining_rows = self
                        .visible_row_count(screen, state)
                        .saturating_sub(self.scroll);
                    if relative >= remaining_rows {
                        return false;
                    }
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
        let row_count = self.visible_row_count(screen, state);
        self.clamp_to_rows(row_count, page);
        (
            self.selected,
            self.scroll,
            self.search_query.clone(),
            self.search_active,
        ) != previous
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
        self.ensure_rows(screen, state);
        let row_cache = self.row_cache.borrow();
        let rows = &row_cache
            .as_ref()
            .expect("public replay rows must be cached before rendering")
            .rows;

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
                browser_projection_description(screen),
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
        let record_noun = if rows.len() == 1 { "record" } else { "records" };
        let title = format!(
            " {} {record_noun} · sanitized public demo details ",
            rows.len()
        );
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

const fn browser_projection_description(screen: MailScreenId) -> &'static str {
    match screen {
        MailScreenId::Dashboard => {
            "Production DashboardScreen driven by the sanitized replay state."
        }
        MailScreenId::Messages => {
            "Synthetic public message events; mailbox reads and mutations are unavailable."
        }
        MailScreenId::Threads => "Synthetic public messages grouped by replay thread identifier.",
        MailScreenId::Agents => {
            "Synthetic public agent roster backed by aggregate-safe replay data."
        }
        MailScreenId::Search => "Client-side text filter across sanitized public replay events.",
        MailScreenId::Reservations => {
            "Synthetic reservation rows; live lock operations are unavailable."
        }
        MailScreenId::ToolMetrics => {
            "Synthetic tool, HTTP, and Git telemetry from the public replay."
        }
        MailScreenId::SystemHealth => {
            "Read-only aggregate health summary from the public replay state."
        }
        MailScreenId::Timeline => {
            "Chronological sanitized replay events with Dashboard deep-link focus."
        }
        MailScreenId::Projects => {
            "Synthetic project labels paired with count-only aggregate metrics."
        }
        MailScreenId::Contacts => {
            "Synthetic public contact graph rows; approval actions are unavailable."
        }
        MailScreenId::Explorer => "Read-only event explorer over the sanitized replay stream.",
        MailScreenId::Analytics => {
            "Derived summary of synthetic telemetry kinds and request counters."
        }
        MailScreenId::Attachments => {
            "The public replay deliberately contains no attachment payload metadata."
        }
        MailScreenId::ArchiveBrowser => {
            "Released synthetic reservations only; Git/archive payloads are not shipped."
        }
        MailScreenId::Atc => {
            "Synthetic coordination roster only; no readiness or authority decision is asserted."
        }
    }
}

fn public_rows(screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
    match screen {
        MailScreenId::Messages => public_event_rows(screen, state),
        MailScreenId::Threads => {
            let mut threads: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
            for event in state.tick_events_since_limited(0, 2_000) {
                let (thread_id, subject, project) = match event {
                    MailEvent::MessageSent {
                        thread_id,
                        subject,
                        project,
                        ..
                    }
                    | MailEvent::MessageReceived {
                        thread_id,
                        subject,
                        project,
                        ..
                    } => (thread_id, subject, project),
                    _ => continue,
                };
                let entry = threads
                    .entry(thread_id)
                    .or_insert_with(|| (subject, project, 0));
                entry.2 = entry.2.saturating_add(1);
            }
            threads
                .into_iter()
                .map(|(thread, (subject, project, count))| {
                    format!("◉ {thread:<26} messages:{count:<4} {subject} · {project}")
                })
                .collect()
        }
        MailScreenId::Attachments => vec![
            "No attachment metadata or payload bytes are included in this public replay."
                .to_string(),
        ],
        MailScreenId::Agents => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .agents_list
            .into_iter()
            .map(|agent| {
                format!(
                    "● {:<20} {:<14} {:<16} {}",
                    agent.name, agent.program, agent.model, agent.project
                )
            })
            .collect(),
        MailScreenId::Projects => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .projects_list
            .into_iter()
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
        MailScreenId::Contacts => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .contacts_list
            .into_iter()
            .map(|contact| {
                format!(
                    "◉ {:<18} → {:<18} {:<10} {}",
                    contact.from_agent, contact.to_agent, contact.status, contact.reason
                )
            })
            .collect(),
        MailScreenId::Reservations => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .reservation_snapshots
            .into_iter()
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
        MailScreenId::ArchiveBrowser => {
            let rows = state
                .db_stats_snapshot()
                .unwrap_or_default()
                .reservation_snapshots
                .into_iter()
                .filter(|reservation| reservation.released_ts.is_some())
                .map(|reservation| {
                    format!(
                        "released {:<18} {:<26} {}",
                        reservation.agent_name, reservation.project_slug, reservation.path_pattern
                    )
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![
                    "No released reservation records are present in this replay phase.".to_string(),
                ]
            } else {
                rows
            }
        }
        MailScreenId::SystemHealth => {
            let stats = state.db_stats_snapshot().unwrap_or_default();
            vec![
                format!(
                    "Database snapshot     projects:{:<5} agents:{:<6} messages:{}",
                    stats.projects, stats.agents, stats.messages
                ),
                format!("Reservation ledger    active:{}", stats.file_reservations),
                format!("Contact graph         links:{}", stats.contact_links),
                format!("Acknowledgements      pending:{}", stats.ack_pending),
                format!("Transport             {}", state.transport_mode_label()),
                format!("Request latency       avg:{}ms", state.avg_latency_ms()),
            ]
        }
        MailScreenId::Atc => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .agents_list
            .into_iter()
            .take(200)
            .map(|agent| {
                format!(
                    "● {:<20} {:<22} {:<14} {}",
                    agent.name, agent.project, agent.program, agent.model
                )
            })
            .collect(),
        MailScreenId::Analytics => {
            let mut counts = BTreeMap::<&'static str, usize>::new();
            for event in state.tick_events_since_limited(0, 2_000) {
                if event_visible_on(screen, &event) {
                    let label = event.kind().compact_label();
                    *counts.entry(label).or_default() += 1;
                }
            }
            let requests = state.request_counters();
            let mut rows = counts
                .into_iter()
                .map(|(kind, count)| format!("telemetry {:<18} events:{count}", kind))
                .collect::<Vec<_>>();
            rows.push(format!(
                "requests total:{} 2xx:{} 4xx:{} 5xx:{} avg:{}ms",
                requests.total,
                requests.status_2xx,
                requests.status_4xx,
                requests.status_5xx,
                state.avg_latency_ms()
            ));
            rows
        }
        MailScreenId::ToolMetrics
        | MailScreenId::Search
        | MailScreenId::Timeline
        | MailScreenId::Explorer => public_event_rows(screen, state),
        MailScreenId::Dashboard => Vec::new(),
    }
}

fn public_event_rows(screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
    state
        .tick_events_since_limited(0, 2_000)
        .into_iter()
        .filter(|event| event_visible_on(screen, event))
        .map(|event| public_event_row(&event))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{MailScreenId, PublicReplayScreen, public_rows};
    use crate::demo_pack::{DemoOperation, curated_public_demo};
    use crate::state::TuiSharedState;
    use ftui::layout::Rect;
    use ftui::{Event, MouseButton, MouseEvent, MouseEventKind};

    fn populated_state() -> TuiSharedState {
        let pack = curated_public_demo();
        let state = TuiSharedState::new();
        pack.apply_bootstrap(&state);
        for (index, action) in pack.actions.iter().enumerate() {
            if action.at_ms != 0 {
                break;
            }
            if matches!(&action.operation, DemoOperation::PublishEvent { .. }) {
                pack.apply_action(index, &state);
            }
        }
        state
    }

    #[test]
    fn public_projections_are_screen_specific_and_authority_bounded() {
        let state = populated_state();
        let messages = public_rows(MailScreenId::Messages, &state);
        let threads = public_rows(MailScreenId::Threads, &state);
        let tools = public_rows(MailScreenId::ToolMetrics, &state);
        let attachments = public_rows(MailScreenId::Attachments, &state);
        let atc = public_rows(MailScreenId::Atc, &state);

        assert!(!messages.is_empty());
        assert!(!threads.is_empty());
        assert_ne!(threads, messages);
        assert!(threads.iter().all(|row| row.contains("messages:")));
        assert!(!tools.is_empty(), "the tool screen must not open blank");
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].contains("No attachment metadata"));
        assert!(atc.iter().all(|row| {
            let lower = row.to_ascii_lowercase();
            !["hold", "clear", "evidence", "conflict"]
                .iter()
                .any(|claim| lower.contains(claim))
        }));
    }

    #[test]
    fn clicking_below_the_last_visible_row_is_a_noop() {
        let state = populated_state();
        let row_count = public_rows(MailScreenId::Messages, &state).len();
        assert!(row_count >= 2);

        let mut screen = PublicReplayScreen::new();
        screen.list_area.set(Rect::new(10, 5, 80, 5));
        screen.scroll = row_count - 2;

        let blank_click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            12,
            9,
        ));
        assert!(!screen.update(&blank_click, MailScreenId::Messages, &state));
        assert_eq!(screen.selected_row(), 0);

        let last_row_click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            12,
            6,
        ));
        assert!(screen.update(&last_row_click, MailScreenId::Messages, &state));
        assert_eq!(screen.selected_row(), row_count - 1);
    }

    #[test]
    fn timeline_focus_selects_the_nearest_event_timestamp() {
        let state = populated_state();
        let events = state.tick_events_since_limited(0, 2_000);
        let target_index = 37;
        let target = events[target_index].timestamp_micros();
        let mut screen = PublicReplayScreen::new();
        screen.list_area.set(Rect::new(0, 0, 120, 12));

        assert!(screen.focus_timeline_at(target, &state));
        assert_eq!(screen.selected_row(), target_index);
    }
}
