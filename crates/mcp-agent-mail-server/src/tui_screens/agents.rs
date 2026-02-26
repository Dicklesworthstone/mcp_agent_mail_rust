//! Agents screen — sortable/filterable roster of registered agents with
//! summary band, status badges, sparkline, responsive columns, and footer.

use std::collections::{HashMap, HashSet, VecDeque};

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::fancy::SummaryFooter;
use crate::tui_widgets::{MetricTile, MetricTrend};

/// Column indices for sorting.
const COL_NAME: usize = 0;
const COL_PROGRAM: usize = 1;
const COL_MODEL: usize = 2;
const COL_LAST_ACTIVE: usize = 3;
const COL_MESSAGES: usize = 4;

const SORT_LABELS: &[&str] = &["Name", "Program", "Model", "Active", "Msgs"];
const STATUS_FADE_TICKS: u8 = 5;
const MESSAGE_FLASH_TICKS: u8 = 3;
const STAGGER_MAX_TICKS: u8 = 10;
const ACTIVE_WINDOW_MICROS: i64 = 60 * 1_000_000;
const IDLE_WINDOW_MICROS: i64 = 5 * 60 * 1_000_000;
/// Max sparkline history samples.
const SPARKLINE_CAP: usize = 30;

/// An agent row with computed fields.
#[derive(Debug, Clone)]
struct AgentRow {
    name: String,
    program: String,
    model: String,
    last_active_ts: i64,
    message_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Active,
    Idle,
    Inactive,
}

impl AgentStatus {
    const fn from_last_active(last_active_ts: i64, now_ts: i64) -> Self {
        if last_active_ts <= 0 {
            return Self::Inactive;
        }
        let elapsed = now_ts.saturating_sub(last_active_ts);
        if elapsed <= ACTIVE_WINDOW_MICROS {
            Self::Active
        } else if elapsed <= IDLE_WINDOW_MICROS {
            Self::Idle
        } else {
            Self::Inactive
        }
    }

    fn rgb(self) -> (u8, u8, u8) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let c = match self {
            Self::Active => tp.activity_active,
            Self::Idle => tp.activity_idle,
            Self::Inactive => tp.activity_stale,
        };
        (c.r(), c.g(), c.b())
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Active => "\u{25CF}",   // ●
            Self::Idle => "\u{25D0}",     // ◐
            Self::Inactive => "\u{25CB}", // ○
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StatusFadeState {
    from: AgentStatus,
    to: AgentStatus,
    ticks_remaining: u8,
}

impl StatusFadeState {
    const fn new(from: AgentStatus, to: AgentStatus) -> Self {
        Self {
            from,
            to,
            ticks_remaining: STATUS_FADE_TICKS,
        }
    }

    const fn step(&mut self) -> bool {
        if self.ticks_remaining > 0 {
            self.ticks_remaining -= 1;
        }
        self.ticks_remaining == 0
    }
}

fn blend_rgb(from: (u8, u8, u8), to: (u8, u8, u8), progress: f32) -> (u8, u8, u8) {
    let t = progress.clamp(0.0, 1.0);
    let blend = |start: u8, end: u8| -> u8 {
        let start = f32::from(start);
        let end = f32::from(end);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (end - start).mul_add(t, start).round() as u8
        }
    };
    (
        blend(from.0, to.0),
        blend(from.1, to.1),
        blend(from.2, to.2),
    )
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

const fn trend_for(current: u64, previous: u64) -> MetricTrend {
    if current > previous {
        MetricTrend::Up
    } else if current < previous {
        MetricTrend::Down
    } else {
        MetricTrend::Flat
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AgentsScreen {
    table_state: TableState,
    agents: Vec<AgentRow>,
    sort_col: usize,
    sort_asc: bool,
    filter: String,
    filter_active: bool,
    last_seq: u64,
    /// Per-agent message counts from events.
    msg_counts: HashMap<String, u64>,
    /// Per-agent model names from `AgentRegistered` events.
    model_names: HashMap<String, String>,
    /// Last computed presence status for each known agent.
    status_by_agent: HashMap<String, AgentStatus>,
    /// Fade transition state when an agent status changes.
    status_fades: HashMap<String, StatusFadeState>,
    /// Brief row highlight when a message event is observed for an agent.
    message_flash_ticks: HashMap<String, u8>,
    /// New rows reveal with a staggered delay to avoid hard pop-in.
    stagger_reveal_ticks: HashMap<String, u8>,
    /// Last row set, used to detect newly appearing agents.
    seen_agents: HashSet<String>,
    /// Reduced-motion mode skips all per-tick visual interpolation.
    reduced_motion: bool,
    /// Synthetic event for the focused agent (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
    /// Sparkline: messages per tick, bounded to `SPARKLINE_CAP` samples.
    msg_rate_history: VecDeque<f64>,
    /// Previous total message count for trend computation.
    prev_total_msgs: u64,
    /// Messages accumulated this tick interval.
    total_msgs_this_tick: u64,
    /// Whether the detail panel is visible on wide screens (user toggle).
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
}

impl AgentsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            agents: Vec::new(),
            sort_col: COL_LAST_ACTIVE,
            sort_asc: false,
            filter: String::new(),
            filter_active: false,
            last_seq: 0,
            msg_counts: HashMap::new(),
            model_names: HashMap::new(),
            status_by_agent: HashMap::new(),
            status_fades: HashMap::new(),
            message_flash_ticks: HashMap::new(),
            stagger_reveal_ticks: HashMap::new(),
            seen_agents: HashSet::new(),
            reduced_motion: reduced_motion_enabled(),
            focused_synthetic: None,
            msg_rate_history: VecDeque::with_capacity(SPARKLINE_CAP),
            prev_total_msgs: 0,
            total_msgs_this_tick: 0,
            detail_visible: true,
            detail_scroll: 0,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected agent.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self
            .table_state
            .selected
            .and_then(|i| self.agents.get(i))
            .map(|row| {
                crate::tui_events::MailEvent::agent_registered(
                    &row.name,
                    &row.program,
                    &row.model,
                    "", // agents span projects
                )
            });
    }

    fn rebuild_from_state(&mut self, state: &TuiSharedState) {
        let db = state.db_stats_snapshot().unwrap_or_default();
        let mut rows: Vec<AgentRow> = db
            .agents_list
            .iter()
            .map(|a| AgentRow {
                name: a.name.clone(),
                program: a.program.clone(),
                model: self.model_names.get(&a.name).cloned().unwrap_or_default(),
                last_active_ts: a.last_active_ts,
                message_count: self.msg_counts.get(&a.name).copied().unwrap_or(0),
            })
            .collect();

        // Apply filter
        if !self.filter.is_empty() {
            let f = self.filter.to_lowercase();
            rows.retain(|r| {
                r.name.to_lowercase().contains(&f)
                    || r.program.to_lowercase().contains(&f)
                    || r.model.to_lowercase().contains(&f)
            });
        }

        // Sort
        rows.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_NAME => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                COL_PROGRAM => a.program.to_lowercase().cmp(&b.program.to_lowercase()),
                COL_MODEL => a.model.to_lowercase().cmp(&b.model.to_lowercase()),
                COL_LAST_ACTIVE => a.last_active_ts.cmp(&b.last_active_ts),
                COL_MESSAGES => a.message_count.cmp(&b.message_count),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });

        self.track_stagger_reveals(&rows);
        self.rebuild_status_transitions(&rows);
        self.agents = rows;

        // Clamp selection
        if let Some(sel) = self.table_state.selected
            && sel >= self.agents.len()
        {
            self.table_state.selected = if self.agents.is_empty() {
                None
            } else {
                Some(self.agents.len() - 1)
            };
        }
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.events_since(self.last_seq);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            match event {
                MailEvent::MessageSent { from, .. } => {
                    *self.msg_counts.entry(from.clone()).or_insert(0) += 1;
                    self.total_msgs_this_tick += 1;
                    if !self.reduced_motion {
                        self.message_flash_ticks
                            .insert(from.clone(), MESSAGE_FLASH_TICKS);
                    }
                }
                MailEvent::MessageReceived { from, to, .. } => {
                    if !self.reduced_motion {
                        self.message_flash_ticks
                            .insert(from.clone(), MESSAGE_FLASH_TICKS);
                        for recipient in to {
                            self.message_flash_ticks
                                .insert(recipient.clone(), MESSAGE_FLASH_TICKS);
                        }
                    }
                }
                MailEvent::AgentRegistered {
                    name, model_name, ..
                } => {
                    self.model_names.insert(name.clone(), model_name.clone());
                }
                _ => {}
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            return;
        }
        let len = self.agents.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
        self.detail_scroll = 0;
    }

    fn rebuild_status_transitions(&mut self, rows: &[AgentRow]) {
        let now_ts = chrono::Utc::now().timestamp_micros();
        let mut next_statuses = HashMap::with_capacity(rows.len());
        for row in rows {
            let next = AgentStatus::from_last_active(row.last_active_ts, now_ts);
            if !self.reduced_motion
                && let Some(prev) = self.status_by_agent.get(&row.name)
                && *prev != next
            {
                self.status_fades
                    .insert(row.name.clone(), StatusFadeState::new(*prev, next));
            }
            next_statuses.insert(row.name.clone(), next);
        }
        self.status_by_agent = next_statuses;
        if self.reduced_motion {
            self.status_fades.clear();
            return;
        }
        self.status_fades.retain(|name, fade| {
            self.status_by_agent
                .get(name)
                .is_some_and(|status| *status == fade.to)
                && fade.ticks_remaining > 0
        });
    }

    fn advance_status_fades(&mut self) {
        self.status_fades.retain(|_, fade| !fade.step());
    }

    fn track_stagger_reveals(&mut self, rows: &[AgentRow]) {
        let mut next_seen = HashSet::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            if !self.reduced_motion && !self.seen_agents.contains(&row.name) {
                let capped = index.min(usize::from(STAGGER_MAX_TICKS - 1));
                let delay = u8::try_from(capped).map_or(STAGGER_MAX_TICKS, |value| value + 1);
                self.stagger_reveal_ticks.insert(row.name.clone(), delay);
            }
            next_seen.insert(row.name.clone());
        }
        self.seen_agents = next_seen;
        if self.reduced_motion {
            self.stagger_reveal_ticks.clear();
            self.message_flash_ticks.clear();
            return;
        }
        self.stagger_reveal_ticks
            .retain(|name, ticks| self.seen_agents.contains(name) && *ticks > 0);
        self.message_flash_ticks
            .retain(|name, ticks| self.seen_agents.contains(name) && *ticks > 0);
    }

    fn advance_message_flashes(&mut self) {
        self.message_flash_ticks.retain(|_, ticks| {
            if *ticks > 0 {
                *ticks -= 1;
            }
            *ticks > 0
        });
    }

    fn advance_stagger_reveals(&mut self) {
        self.stagger_reveal_ticks.retain(|_, ticks| {
            if *ticks > 0 {
                *ticks -= 1;
            }
            *ticks > 0
        });
    }

    /// Record sparkline data point: messages since last sample.
    fn record_sparkline_sample(&mut self) {
        if self.msg_rate_history.len() >= SPARKLINE_CAP {
            self.msg_rate_history.pop_front();
        }
        let sparkline_sample = u32::try_from(self.total_msgs_this_tick).unwrap_or(u32::MAX);
        self.msg_rate_history.push_back(f64::from(sparkline_sample));
        self.total_msgs_this_tick = 0;
    }

    fn status_color(&self, agent: &AgentRow, now_ts: i64) -> PackedRgba {
        let target = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
        if self.reduced_motion {
            let (r, g, b) = target.rgb();
            return PackedRgba::rgb(r, g, b);
        }
        if let Some(fade) = self.status_fades.get(&agent.name) {
            let progress =
                1.0 - (f32::from(fade.ticks_remaining) / f32::from(STATUS_FADE_TICKS.max(1)));
            let (r, g, b) = blend_rgb(fade.from.rgb(), fade.to.rgb(), progress);
            return PackedRgba::rgb(r, g, b);
        }
        let (r, g, b) = target.rgb();
        PackedRgba::rgb(r, g, b)
    }

    fn row_style(&self, row_index: usize, agent: &AgentRow, now_ts: i64) -> Style {
        let tp = crate::tui_theme::TuiThemePalette::current();
        if Some(row_index) == self.table_state.selected {
            return Style::default().fg(tp.selection_fg).bg(tp.selection_bg);
        }
        if !self.reduced_motion && self.stagger_reveal_ticks.contains_key(&agent.name) {
            return crate::tui_theme::text_disabled(&tp);
        }

        let status_color = self.status_color(agent, now_ts);
        let mut style = Style::default().fg(status_color);
        if !self.reduced_motion
            && let Some(remaining) = self.message_flash_ticks.get(&agent.name)
        {
            let intensity = f32::from(*remaining) / f32::from(MESSAGE_FLASH_TICKS.max(1));
            let dim = (tp.text_muted.r(), tp.text_muted.g(), tp.text_muted.b());
            let bright = (
                tp.selection_bg.r(),
                tp.selection_bg.g(),
                tp.selection_bg.b(),
            );
            let (r, g, b) = blend_rgb(dim, bright, intensity);
            style = style.bg(PackedRgba::rgb(r, g, b)).fg(tp.selection_fg);
        }
        style
    }

    /// Count agents by status category.
    fn status_counts(&self) -> (u64, u64, u64) {
        let now_ts = chrono::Utc::now().timestamp_micros();
        let mut active = 0u64;
        let mut idle = 0u64;
        let mut inactive = 0u64;
        for agent in &self.agents {
            match AgentStatus::from_last_active(agent.last_active_ts, now_ts) {
                AgentStatus::Active => active += 1,
                AgentStatus::Idle => idle += 1,
                AgentStatus::Inactive => inactive += 1,
            }
        }
        (active, idle, inactive)
    }
}

impl Default for AgentsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for AgentsScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            // Filter mode: capture text input
            if self.filter_active {
                match key.code {
                    KeyCode::Escape | KeyCode::Enter => {
                        self.filter_active = false;
                    }
                    KeyCode::Backspace => {
                        self.filter.pop();
                        self.rebuild_from_state(state);
                    }
                    KeyCode::Char(c) => {
                        self.filter.push(c);
                        self.rebuild_from_state(state);
                    }
                    _ => {}
                }
                return Cmd::None;
            }

            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Char('G') | KeyCode::End => {
                    if !self.agents.is_empty() {
                        self.table_state.selected = Some(self.agents.len() - 1);
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if !self.agents.is_empty() {
                        self.table_state.selected = Some(0);
                    }
                }
                KeyCode::Char('/') => {
                    self.filter_active = true;
                    self.filter.clear();
                }
                KeyCode::Char('s') => {
                    self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                    self.rebuild_from_state(state);
                }
                KeyCode::Char('S') => {
                    self.sort_asc = !self.sort_asc;
                    self.rebuild_from_state(state);
                }
                KeyCode::Char('i') => {
                    self.detail_visible = !self.detail_visible;
                }
                KeyCode::Char('J') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Escape => {
                    if !self.filter.is_empty() {
                        self.filter.clear();
                        self.rebuild_from_state(state);
                    }
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.ingest_events(state);
        if !self.reduced_motion {
            self.advance_status_fades();
            self.advance_message_flashes();
            self.advance_stagger_reveals();
        }
        // Rebuild every second (10 ticks)
        if tick_count.is_multiple_of(10) {
            let total: u64 = self.msg_counts.values().sum();
            self.prev_total_msgs = total;
            self.record_sparkline_sample();
            self.rebuild_from_state(state);
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();

        // Outer bordered panel wrapping entire screen content
        let outer_block = crate::tui_panel_helpers::panel_block(" Registered Agents ");
        let inner = outer_block.inner(area);
        outer_block.render(area, frame);
        let area = inner;

        // Responsive layout: single-col on narrow, table+detail on wide
        let layout = ResponsiveLayout::new(
            Flex::vertical().constraints([Constraint::Fill]),
        )
        .at(
            Breakpoint::Lg,
            Flex::horizontal().constraints([
                Constraint::Percentage(60.0),
                Constraint::Fill,
            ]),
        )
        .at(
            Breakpoint::Xl,
            Flex::horizontal().constraints([
                Constraint::Percentage(50.0),
                Constraint::Fill,
            ]),
        );

        let split = layout.split(area);
        let table_area = split.rects[0];

        self.render_table_content(frame, table_area, &tp);

        // Render detail panel if visible (Lg+)
        if split.rects.len() >= 2 && self.detail_visible {
            self.render_detail_panel(frame, split.rects[1]);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Select agent",
            },
            HelpEntry {
                key: "/",
                action: "Search/filter",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort order",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
            HelpEntry {
                key: "Esc",
                action: "Clear filter",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Registered agents and their status. Enter to view inbox, / to filter.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::AgentByName(name) = target
            && let Some(pos) = self.agents.iter().position(|a| a.name == *name)
        {
            self.table_state.selected = Some(pos);
            return true;
        }
        false
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_active
    }

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let agent = self.agents.get(idx)?;
        Some(agent.name.clone())
    }

    fn title(&self) -> &'static str {
        "Agents"
    }

    fn tab_label(&self) -> &'static str {
        "Agents"
    }
}

// ── Rendering helpers ──────────────────────────────────────────────────

impl AgentsScreen {
    /// Render summary band + header + table + footer into a single column area.
    fn render_table_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        tp: &crate::tui_theme::TuiThemePalette,
    ) {
        let wide = area.width >= 120;
        let narrow = area.width < 80;

        // Layout: summary(2) + header(1) + table(remainder) + footer(1)
        let summary_h: u16 = if area.height >= 8 { 2 } else { 0 };
        let header_h: u16 = 1;
        let footer_h = u16::from(area.height >= 6);
        let table_h = area
            .height
            .saturating_sub(summary_h)
            .saturating_sub(header_h)
            .saturating_sub(footer_h);

        let mut y = area.y;

        // ── Summary band ───────────────────────────────────────────────
        if summary_h > 0 {
            let summary_area = Rect::new(area.x, y, area.width, summary_h);
            self.render_summary_band(frame, summary_area);
            y += summary_h;
        }

        // ── Info header ────────────────────────────────────────────────
        let header_area = Rect::new(area.x, y, area.width, header_h);
        y += header_h;

        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
            .render(header_area, frame);

        let sort_indicator = if self.sort_asc {
            " \u{25b2}"
        } else {
            " \u{25bc}"
        };
        let sort_label = SORT_LABELS.get(self.sort_col).unwrap_or(&"?");
        let filter_display = if self.filter_active {
            format!(" [/] Search: {}_ ", self.filter)
        } else if !self.filter.is_empty() {
            format!(" [/] Filter: {} ", self.filter)
        } else {
            String::new()
        };
        let info = format!(
            "{} agents | Sort: {}{} {}",
            self.agents.len(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        Paragraph::new(info).render(header_area, frame);

        // ── Table ──────────────────────────────────────────────────────
        let table_area = Rect::new(area.x, y, area.width, table_h);
        y += table_h;

        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
            .render(table_area, frame);

        self.render_table(frame, table_area, wide, narrow);

        // ── Footer ─────────────────────────────────────────────────────
        if footer_h > 0 {
            let footer_area = Rect::new(area.x, y, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }
    }

    /// Render the detail panel for the currently selected agent.
    fn render_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Agent Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(selected_idx) = self.table_state.selected else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f464}",
                "No Agent Selected",
                "Select an agent from the table to view details.",
            );
            return;
        };

        let Some(agent) = self.agents.get(selected_idx) else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f464}",
                "No Agent Selected",
                "Select an agent from the table to view details.",
            );
            return;
        };

        let lines = self.build_detail_lines(agent, &tp);
        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }

    /// Build key-value lines for agent detail.
    fn build_detail_lines(
        &self,
        agent: &AgentRow,
        tp: &crate::tui_theme::TuiThemePalette,
    ) -> Vec<(String, String, Option<PackedRgba>)> {
        let now_ts = chrono::Utc::now().timestamp_micros();
        let status = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
        let status_color = self.status_color(agent, now_ts);

        let mut lines: Vec<(String, String, Option<PackedRgba>)> = Vec::new();

        lines.push(("Name".into(), agent.name.clone(), None));
        lines.push((
            "Status".into(),
            format!(
                "{} {}",
                status.icon(),
                match status {
                    AgentStatus::Active => "Active",
                    AgentStatus::Idle => "Idle",
                    AgentStatus::Inactive => "Inactive",
                }
            ),
            Some(status_color),
        ));
        lines.push(("Program".into(), agent.program.clone(), None));
        if !agent.model.is_empty() {
            lines.push(("Model".into(), agent.model.clone(), None));
        }
        lines.push((
            "Last Active".into(),
            if agent.last_active_ts == 0 {
                "never".into()
            } else {
                let relative = format_relative_time(agent.last_active_ts);
                let iso = mcp_agent_mail_db::timestamps::micros_to_iso(agent.last_active_ts);
                format!("{relative}  ({iso})")
            },
            None,
        ));
        lines.push(("Messages".into(), agent.message_count.to_string(), None));

        // Sparkline as text bar
        if !self.msg_rate_history.is_empty() {
            lines.push((
                "Msg Rate".into(),
                sparkline_bar(&self.msg_rate_history),
                Some(tp.metric_messages),
            ));
        }

        lines
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_summary_band(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let total_agents = self.agents.len() as u64;
        let (active, idle, _inactive) = self.status_counts();
        let total_msgs: u64 = self.msg_counts.values().sum();

        let agents_str = total_agents.to_string();
        let active_str = active.to_string();
        let idle_str = idle.to_string();
        let msgs_str = total_msgs.to_string();

        let sparkline_data: Vec<f64> = self.msg_rate_history.iter().copied().collect();

        let tiles: Vec<(&str, &str, MetricTrend, PackedRgba)> = vec![
            ("Agents", &agents_str, MetricTrend::Flat, tp.metric_agents),
            ("Active", &active_str, MetricTrend::Flat, tp.activity_active),
            ("Idle", &idle_str, MetricTrend::Flat, tp.activity_idle),
            (
                "Messages",
                &msgs_str,
                trend_for(total_msgs, self.prev_total_msgs),
                tp.metric_messages,
            ),
        ];

        let tile_count = tiles.len();
        if tile_count == 0 || area.width == 0 || area.height == 0 {
            return;
        }
        let tile_w = area.width / tile_count as u16;

        for (i, (label, value, trend, color)) in tiles.iter().enumerate() {
            let x = area.x + (i as u16) * tile_w;
            let w = if i == tile_count - 1 {
                area.width.saturating_sub(x - area.x)
            } else {
                tile_w
            };
            let tile_area = Rect::new(x, area.y, w, area.height);
            let mut tile = MetricTile::new(label, value, *trend)
                .value_color(*color)
                .sparkline_color(*color);
            // Add sparkline to Messages tile
            if *label == "Messages" && !sparkline_data.is_empty() {
                tile = tile.sparkline(&sparkline_data);
            }
            tile.render(tile_area, frame);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_table(&self, frame: &mut Frame<'_>, area: Rect, wide: bool, narrow: bool) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let now_ts = chrono::Utc::now().timestamp_micros();

        // Responsive columns
        let (header_cells, widths): (Vec<&str>, Vec<Constraint>) = if narrow {
            // < 80: Name(+status), Active, Msgs only
            (
                vec!["Name", "Last Active", "Msgs"],
                vec![
                    Constraint::Percentage(40.0),
                    Constraint::Percentage(30.0),
                    Constraint::Percentage(30.0),
                ],
            )
        } else if wide {
            // >= 120: all columns + status badge
            (
                vec!["Name", "Program", "Model", "Last Active", "Msgs"],
                vec![
                    Constraint::Percentage(25.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(15.0),
                ],
            )
        } else {
            // 80–119: hide Model
            (
                vec!["Name", "Program", "Last Active", "Msgs"],
                vec![
                    Constraint::Percentage(28.0),
                    Constraint::Percentage(25.0),
                    Constraint::Percentage(27.0),
                    Constraint::Percentage(20.0),
                ],
            )
        };

        let header = Row::new(header_cells).style(Style::default().bold());

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let active_str = format_relative_time(agent.last_active_ts);
                let msg_str = agent.message_count.to_string();
                let style = self.row_style(i, agent, now_ts);

                // Status badge prepended to name
                let status = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
                let name_display = format!("{} {}", status.icon(), agent.name);

                if narrow {
                    Row::new([name_display, active_str, msg_str]).style(style)
                } else if wide {
                    Row::new([
                        name_display,
                        agent.program.clone(),
                        agent.model.clone(),
                        active_str,
                        msg_str,
                    ])
                    .style(style)
                } else {
                    Row::new([name_display, agent.program.clone(), active_str, msg_str])
                        .style(style)
                }
            })
            .collect();

        let block = Block::default()
            .title("Agents")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, area, frame, &mut ts);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let total = self.agents.len() as u64;
        let (active, idle, inactive) = self.status_counts();
        let total_msgs: u64 = self.msg_counts.values().sum();

        let total_str = total.to_string();
        let active_str = active.to_string();
        let idle_str = idle.to_string();
        let inactive_str = inactive.to_string();
        let msgs_str = total_msgs.to_string();

        let items: Vec<(&str, &str, PackedRgba)> = vec![
            (&*total_str, "agents", tp.metric_agents),
            (&*active_str, "active", tp.activity_active),
            (&*idle_str, "idle", tp.activity_idle),
            (&*inactive_str, "offline", tp.activity_stale),
            (&*msgs_str, "msgs", tp.metric_messages),
        ];

        SummaryFooter::new(&items, tp.text_muted).render(area, frame);
    }
}

/// Format a timestamp as relative time from now.
fn format_relative_time(ts_micros: i64) -> String {
    if ts_micros == 0 {
        return "never".to_string();
    }
    let now = chrono::Utc::now().timestamp_micros();
    let delta_secs = (now - ts_micros) / 1_000_000;
    if delta_secs < 0 {
        return "future".to_string();
    }
    let delta = delta_secs.unsigned_abs();
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

const SPARKLINE_BLOCKS: &[char] = &[
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
];

/// Build a text-mode sparkline bar from rate history.
fn sparkline_bar(history: &VecDeque<f64>) -> String {
    let max_val = history
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(1.0);
    history
        .iter()
        .map(|v| {
            let frac = v / max_val;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let idx = (frac * 7.0).round() as usize;
            SPARKLINE_BLOCKS[idx.min(7)]
        })
        .collect()
}

/// Render key-value lines with a label column and a value column, supporting scroll.
#[allow(clippy::cast_possible_truncation)]
fn render_kv_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: &[(String, String, Option<PackedRgba>)],
    scroll: usize,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    let visible_height = usize::from(inner.height);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = scroll.min(max_scroll);
    let label_w = 12u16;

    for (i, (label, value, color)) in lines.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Label
        let label_area = Rect::new(inner.x, y, label_w.min(inner.width), 1);
        let label_text = format!("{label}:");
        Paragraph::new(label_text)
            .style(Style::default().fg(tp.text_muted).bold())
            .render(label_area, frame);

        // Value
        let val_x = inner.x + label_w + 1;
        if val_x < inner.x + inner.width {
            let val_w = (inner.x + inner.width).saturating_sub(val_x);
            let val_area = Rect::new(val_x, y, val_w, 1);
            let val_style = color.map_or_else(
                || Style::default().fg(tp.text_primary),
                |c| Style::default().fg(c),
            );
            Paragraph::new(value.as_str())
                .style(val_style)
                .render(val_area, frame);
        }
    }

    // Scroll indicator
    if total_lines > visible_height {
        let indicator = format!(
            " {}/{} ",
            scroll + 1,
            total_lines.saturating_sub(visible_height) + 1
        );
        let ind_w = indicator.len() as u16;
        if ind_w < inner.width {
            let ind_area = Rect::new(
                inner.x + inner.width - ind_w,
                inner.y + inner.height.saturating_sub(1),
                ind_w,
                1,
            );
            Paragraph::new(indicator)
                .style(Style::default().fg(tp.text_muted))
                .render(ind_area, frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    #[test]
    fn new_screen_has_defaults() {
        let screen = AgentsScreen::new();
        assert!(screen.agents.is_empty());
        assert!(!screen.filter_active);
        assert_eq!(screen.sort_col, COL_LAST_ACTIVE);
        assert!(!screen.sort_asc);
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 3), &state);
    }

    #[test]
    fn renders_at_tiny_size_without_panic() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn renders_wide_layout() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(140, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 140, 30), &state);
    }

    #[test]
    fn renders_narrow_layout() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = AgentsScreen::new();
        assert_eq!(screen.title(), "Agents");
        assert_eq!(screen.tab_label(), "Agents");
    }

    #[test]
    fn keybindings_documented() {
        let screen = AgentsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 4);
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "/"));
    }

    #[test]
    fn slash_activates_filter() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        assert!(!screen.consumes_text_input());

        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn escape_deactivates_filter() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);
        assert!(!screen.consumes_text_input());
    }

    #[test]
    fn s_cycles_sort_column() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let initial = screen.sort_col;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn big_s_toggles_sort_order() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let initial = screen.sort_asc;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('S')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_asc, initial);
    }

    #[test]
    fn deep_link_agent_by_name() {
        let mut screen = AgentsScreen::new();
        screen.agents.push(AgentRow {
            name: "RedFox".to_string(),
            program: "claude-code".to_string(),
            model: "opus-4.6".to_string(),
            last_active_ts: 100,
            message_count: 5,
        });
        let handled = screen.receive_deep_link(&DeepLinkTarget::AgentByName("RedFox".into()));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn deep_link_unknown_agent() {
        let mut screen = AgentsScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::AgentByName("Unknown".into()));
        assert!(!handled);
    }

    #[test]
    fn format_relative_time_values() {
        assert_eq!(format_relative_time(0), "never");
        let now = chrono::Utc::now().timestamp_micros();
        let result = format_relative_time(now - 30_000_000); // 30s ago
        assert!(result.contains("s ago"));
        let result = format_relative_time(now - 300_000_000); // 5m ago
        assert!(result.contains("m ago"));
    }

    #[test]
    fn default_impl() {
        let screen = AgentsScreen::default();
        assert!(screen.agents.is_empty());
    }

    #[test]
    fn status_thresholds_are_classified() {
        let now = chrono::Utc::now().timestamp_micros();
        assert_eq!(AgentStatus::from_last_active(now, now), AgentStatus::Active);
        assert_eq!(
            AgentStatus::from_last_active(now - ACTIVE_WINDOW_MICROS - 1, now),
            AgentStatus::Idle
        );
        assert_eq!(
            AgentStatus::from_last_active(now - IDLE_WINDOW_MICROS - 1, now),
            AgentStatus::Inactive
        );
        assert_eq!(AgentStatus::from_last_active(0, now), AgentStatus::Inactive);
    }

    #[test]
    fn status_fade_records_transition_and_expires() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        let now = chrono::Utc::now().timestamp_micros();
        let mut rows = vec![AgentRow {
            name: "RedFox".to_string(),
            program: "claude-code".to_string(),
            model: "opus".to_string(),
            last_active_ts: now,
            message_count: 1,
        }];

        screen.rebuild_status_transitions(&rows);
        assert!(screen.status_fades.is_empty());

        rows[0].last_active_ts = now - IDLE_WINDOW_MICROS - 10_000_000;
        screen.rebuild_status_transitions(&rows);
        let fade = screen
            .status_fades
            .get("RedFox")
            .expect("status transition should create fade");
        assert_eq!(fade.from, AgentStatus::Active);
        assert_eq!(fade.to, AgentStatus::Inactive);
        assert_eq!(fade.ticks_remaining, STATUS_FADE_TICKS);

        for _ in 0..STATUS_FADE_TICKS {
            screen.advance_status_fades();
        }
        assert!(screen.status_fades.is_empty());
    }

    #[test]
    fn reduced_motion_disables_status_fades() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = true;
        let now = chrono::Utc::now().timestamp_micros();
        let mut rows = vec![AgentRow {
            name: "BlueFox".to_string(),
            program: "claude-code".to_string(),
            model: "opus".to_string(),
            last_active_ts: now,
            message_count: 1,
        }];

        screen.rebuild_status_transitions(&rows);
        rows[0].last_active_ts = now - IDLE_WINDOW_MICROS - 10_000_000;
        screen.rebuild_status_transitions(&rows);
        assert!(screen.status_fades.is_empty());
    }

    #[test]
    fn message_flash_ticks_decay_to_zero() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        screen
            .message_flash_ticks
            .insert("RedFox".to_string(), MESSAGE_FLASH_TICKS);

        for _ in 0..MESSAGE_FLASH_TICKS {
            screen.advance_message_flashes();
        }
        assert!(!screen.message_flash_ticks.contains_key("RedFox"));
    }

    #[test]
    fn stagger_reveal_assigns_cascading_delays() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        let now = chrono::Utc::now().timestamp_micros();
        let rows = vec![
            AgentRow {
                name: "A".to_string(),
                program: "p".to_string(),
                model: "m".to_string(),
                last_active_ts: now,
                message_count: 0,
            },
            AgentRow {
                name: "B".to_string(),
                program: "p".to_string(),
                model: "m".to_string(),
                last_active_ts: now,
                message_count: 0,
            },
            AgentRow {
                name: "C".to_string(),
                program: "p".to_string(),
                model: "m".to_string(),
                last_active_ts: now,
                message_count: 0,
            },
        ];

        screen.track_stagger_reveals(&rows);
        assert_eq!(screen.stagger_reveal_ticks.get("A"), Some(&1));
        assert_eq!(screen.stagger_reveal_ticks.get("B"), Some(&2));
        assert_eq!(screen.stagger_reveal_ticks.get("C"), Some(&3));

        screen.advance_stagger_reveals();
        assert!(!screen.stagger_reveal_ticks.contains_key("A"));
        assert_eq!(screen.stagger_reveal_ticks.get("B"), Some(&1));
    }

    // ── focused_event tests ───────────────────────────────────────

    #[test]
    fn focused_event_none_when_empty() {
        let screen = AgentsScreen::new();
        assert!(screen.focused_event().is_none());
    }

    #[test]
    fn focused_event_returns_agent_registered_synthetic() {
        let mut screen = AgentsScreen::new();
        screen.agents.push(AgentRow {
            name: "RedFox".to_string(),
            program: "claude-code".to_string(),
            model: "opus-4.6".to_string(),
            last_active_ts: 0,
            message_count: 0,
        });
        screen.table_state.selected = Some(0);
        screen.sync_focused_event();

        assert!(matches!(
            screen.focused_event(),
            Some(crate::tui_events::MailEvent::AgentRegistered { name, program, .. })
                if name == "RedFox" && program == "claude-code"
        ));
    }

    #[test]
    fn focused_event_none_when_selection_out_of_range() {
        let mut screen = AgentsScreen::new();
        screen.table_state.selected = Some(5);
        screen.sync_focused_event();
        assert!(screen.focused_event().is_none());
    }

    #[test]
    fn sparkline_records_samples() {
        let mut screen = AgentsScreen::new();
        screen.total_msgs_this_tick = 5;
        screen.record_sparkline_sample();
        assert_eq!(screen.msg_rate_history.len(), 1);
        assert!((screen.msg_rate_history[0] - 5.0).abs() < f64::EPSILON);
        assert_eq!(screen.total_msgs_this_tick, 0);
    }

    #[test]
    fn sparkline_bounded_capacity() {
        let mut screen = AgentsScreen::new();
        for i in 0..SPARKLINE_CAP + 5 {
            screen.total_msgs_this_tick = i as u64;
            screen.record_sparkline_sample();
        }
        assert_eq!(screen.msg_rate_history.len(), SPARKLINE_CAP);
    }

    #[test]
    fn status_counts_empty() {
        let screen = AgentsScreen::new();
        assert_eq!(screen.status_counts(), (0, 0, 0));
    }
}
