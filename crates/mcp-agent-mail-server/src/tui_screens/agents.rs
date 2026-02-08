//! Agents screen — sortable/filterable roster of registered agents.

use std::collections::HashMap;

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

/// An agent row with computed fields.
#[derive(Debug, Clone)]
struct AgentRow {
    name: String,
    program: String,
    model: String,
    last_active_ts: i64,
    message_count: u64,
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
        }
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
                    KeyCode::Enter => {
                        if let Some(sel) = self.table_state.selected {
                            if let Some(agent) = self.agents.get(sel) {
                                return Cmd::msg(MailScreenMsg::DeepLink(
                                    DeepLinkTarget::AgentByName(agent.name.clone()),
                                ));
                            }
                        }
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
        // Rebuild every second
        if tick_count % 10 == 0 {
            self.rebuild_from_state(state);
        }
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

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let active_str = format_relative_time(agent.last_active_ts);
                let msg_str = format!("{}", agent.message_count);
                let style = if Some(i) == self.table_state.selected {
                    Style::default()
                        .fg(PackedRgba::rgb(0, 0, 0))
                        .bg(PackedRgba::rgb(120, 220, 150))
                } else {
                    Style::default()
                };
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

        let block = Block::default()
            .title("Agents")
            .border_type(BorderType::Rounded);

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(
                Style::default()
                    .fg(PackedRgba::rgb(0, 0, 0))
                    .bg(PackedRgba::rgb(120, 220, 150)),
            );

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
                key: "Enter",
                action: "View agent timeline",
            },
            HelpEntry {
                key: "Esc",
                action: "Clear filter",
            },
        ]
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
}
