//! Agents screen — sortable/filterable roster of registered agents.

use std::collections::{HashMap, HashSet};

use ftui::layout::Constraint;
use ftui::layout::Rect;
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
const ACTIVE_WINDOW_MICROS: i64 = 2 * 60 * 1_000_000;
const IDLE_WINDOW_MICROS: i64 = 15 * 60 * 1_000_000;

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
        if let Some(sel) = self.table_state.selected {
            if sel >= self.agents.len() {
                self.table_state.selected = if self.agents.is_empty() {
                    None
                } else {
                    Some(self.agents.len() - 1)
                };
            }
        }
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.events_since(self.last_seq);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            match event {
                MailEvent::MessageSent { from, .. } => {
                    *self.msg_counts.entry(from.clone()).or_insert(0) += 1;
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
            return Style::default().fg(tp.text_disabled);
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
}

impl Default for AgentsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for AgentsScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
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
                    KeyCode::Escape => {
                        if !self.filter.is_empty() {
                            self.filter.clear();
                            self.rebuild_from_state(state);
                        }
                    }
                    _ => {}
                }
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
        // Rebuild every second
        if tick_count % 10 == 0 {
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

        // Header bar: 1 line for filter/sort info
        let header_h = 1_u16;
        let table_h = area.height.saturating_sub(header_h);

        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);

        // Render header info line
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
        let p = Paragraph::new(info);
        p.render(header_area, frame);

        // Build table rows
        let header = Row::new(["Name", "Program", "Model", "Last Active", "Msgs"])
            .style(Style::default().bold());
        let now_ts = chrono::Utc::now().timestamp_micros();

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let active_str = format_relative_time(agent.last_active_ts);
                let msg_str = format!("{}", agent.message_count);
                let style = self.row_style(i, agent, now_ts);
                Row::new([
                    agent.name.as_str().to_string(),
                    agent.program.clone(),
                    agent.model.clone(),
                    active_str,
                    msg_str,
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Percentage(25.0),
            Constraint::Percentage(20.0),
            Constraint::Percentage(20.0),
            Constraint::Percentage(20.0),
            Constraint::Percentage(15.0),
        ];

        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Agents")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
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
                key: "Esc",
                action: "Clear filter",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Registered agents and their status. Enter to view inbox, / to filter.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::AgentByName(name) = target {
            if let Some(pos) = self.agents.iter().position(|a| a.name == *name) {
                self.table_state.selected = Some(pos);
                return true;
            }
        }
        false
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_active
    }

    fn title(&self) -> &'static str {
        "Agents"
    }

    fn tab_label(&self) -> &'static str {
        "Agents"
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
}
