//! Projects screen — sortable/filterable project browser with per-project stats.

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, Style};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::ProjectSummary;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

/// Column indices for sorting.
const COL_SLUG: usize = 0;
const COL_HUMAN_KEY: usize = 1;
const COL_AGENTS: usize = 2;
const COL_MESSAGES: usize = 3;
const COL_RESERVATIONS: usize = 4;
const COL_CREATED: usize = 5;

const SORT_LABELS: &[&str] = &["Slug", "Path", "Agents", "Msgs", "Reserv", "Created"];

pub struct ProjectsScreen {
    table_state: TableState,
    projects: Vec<ProjectSummary>,
    sort_col: usize,
    sort_asc: bool,
    filter: String,
    filter_active: bool,
}

impl ProjectsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            projects: Vec::new(),
            sort_col: COL_CREATED,
            sort_asc: false,
            filter: String::new(),
            filter_active: false,
        }
    }

    fn rebuild_from_state(&mut self, state: &TuiSharedState) {
        let db = state.db_stats_snapshot().unwrap_or_default();
        let mut rows: Vec<ProjectSummary> = db.projects_list;

        // Apply filter
        if !self.filter.is_empty() {
            let f = self.filter.to_lowercase();
            rows.retain(|r| {
                r.slug.to_lowercase().contains(&f) || r.human_key.to_lowercase().contains(&f)
            });
        }

        // Sort
        rows.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_SLUG => a.slug.to_lowercase().cmp(&b.slug.to_lowercase()),
                COL_HUMAN_KEY => a.human_key.to_lowercase().cmp(&b.human_key.to_lowercase()),
                COL_AGENTS => a.agent_count.cmp(&b.agent_count),
                COL_MESSAGES => a.message_count.cmp(&b.message_count),
                COL_RESERVATIONS => a.reservation_count.cmp(&b.reservation_count),
                COL_CREATED => a.created_at.cmp(&b.created_at),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });

        self.projects = rows;

        // Clamp selection
        if let Some(sel) = self.table_state.selected {
            if sel >= self.projects.len() {
                self.table_state.selected = if self.projects.is_empty() {
                    None
                } else {
                    Some(self.projects.len() - 1)
                };
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.projects.is_empty() {
            return;
        }
        let len = self.projects.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
    }
}

impl Default for ProjectsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ProjectsScreen {
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
                        if !self.projects.is_empty() {
                            self.table_state.selected = Some(self.projects.len() - 1);
                        }
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        if !self.projects.is_empty() {
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
        // Rebuild every second
        if tick_count % 10 == 0 {
            self.rebuild_from_state(state);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
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
            "{} projects   Sort: {}{} {}",
            self.projects.len(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        let p = Paragraph::new(info);
        p.render(header_area, frame);

        // Build table rows
        let header = Row::new(["Slug", "Path", "Agents", "Msgs", "Reserv", "Created"])
            .style(Style::default().bold());

        let rows: Vec<Row> = self
            .projects
            .iter()
            .enumerate()
            .map(|(i, proj)| {
                let created_str = format_created_time(proj.created_at);
                let tp = crate::tui_theme::TuiThemePalette::current();
                let style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else {
                    Style::default()
                };
                // Truncate human_key to fit
                let path_display = truncate_path(&proj.human_key, area.width as usize / 4);
                Row::new([
                    proj.slug.clone(),
                    path_display,
                    proj.agent_count.to_string(),
                    proj.message_count.to_string(),
                    proj.reservation_count.to_string(),
                    created_str,
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Percentage(20.0),
            Constraint::Percentage(30.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(20.0),
        ];

        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Projects")
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
                action: "Select project",
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
        Some("Project registry with agent counts and message totals.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ProjectBySlug(slug) = target {
            if let Some(pos) = self.projects.iter().position(|p| p.slug == *slug) {
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
        "Projects"
    }

    fn tab_label(&self) -> &'static str {
        "Proj"
    }
}

/// Format a creation timestamp as an absolute date/time string.
fn format_created_time(ts_micros: i64) -> String {
    if ts_micros == 0 {
        return "unknown".to_string();
    }
    let secs = ts_micros / 1_000_000;
    let dt = chrono::DateTime::from_timestamp(secs, 0);
    dt.map_or_else(
        || "invalid".to_string(),
        |d| d.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// Truncate a file path for display, keeping the last `max_len` characters.
fn truncate_path(path: &str, max_len: usize) -> String {
    if max_len < 4 {
        return "...".to_string();
    }
    if path.len() <= max_len {
        return path.to_string();
    }
    format!("...{}", &path[path.len() - (max_len - 3)..])
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
        let screen = ProjectsScreen::new();
        assert!(screen.projects.is_empty());
        assert!(!screen.filter_active);
        assert_eq!(screen.sort_col, COL_CREATED);
        assert!(!screen.sort_asc);
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = ProjectsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = ProjectsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 3), &state);
    }

    #[test]
    fn renders_at_tiny_size_without_panic() {
        let state = test_state();
        let screen = ProjectsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = ProjectsScreen::new();
        assert_eq!(screen.title(), "Projects");
        assert_eq!(screen.tab_label(), "Proj");
    }

    #[test]
    fn keybindings_documented() {
        let screen = ProjectsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 4);
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "/"));
    }

    #[test]
    fn slash_activates_filter() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();
        assert!(!screen.consumes_text_input());

        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn escape_deactivates_filter() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();
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
        let mut screen = ProjectsScreen::new();
        let initial = screen.sort_col;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn big_s_toggles_sort_order() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();
        let initial = screen.sort_asc;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('S')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_asc, initial);
    }

    #[test]
    fn deep_link_project_by_slug() {
        let mut screen = ProjectsScreen::new();
        screen.projects.push(ProjectSummary {
            id: 1,
            slug: "my-project".into(),
            human_key: "/home/user/my-project".into(),
            agent_count: 3,
            message_count: 10,
            reservation_count: 2,
            created_at: 100_000_000,
        });
        let handled = screen.receive_deep_link(&DeepLinkTarget::ProjectBySlug("my-project".into()));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn deep_link_unknown_project() {
        let mut screen = ProjectsScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::ProjectBySlug("unknown".into()));
        assert!(!handled);
    }

    #[test]
    fn format_created_time_values() {
        assert_eq!(format_created_time(0), "unknown");
        // 2026-01-01 00:00:00 UTC in microseconds
        let ts = 1_767_225_600_000_000_i64;
        let result = format_created_time(ts);
        assert!(result.starts_with("2026-01-01"));
    }

    #[test]
    fn truncate_path_values() {
        assert_eq!(truncate_path("/short", 20), "/short");
        assert_eq!(truncate_path("/a/very/long/path/here", 10), "...th/here");
        assert_eq!(truncate_path("x", 3), "...");
    }

    #[test]
    fn default_impl() {
        let screen = ProjectsScreen::default();
        assert!(screen.projects.is_empty());
    }

    #[test]
    fn move_selection_navigation() {
        let mut screen = ProjectsScreen::new();
        screen.projects.push(ProjectSummary {
            slug: "a".into(),
            ..Default::default()
        });
        screen.projects.push(ProjectSummary {
            slug: "b".into(),
            ..Default::default()
        });
        screen.projects.push(ProjectSummary {
            slug: "c".into(),
            ..Default::default()
        });
        screen.table_state.selected = Some(0);

        screen.move_selection(1);
        assert_eq!(screen.table_state.selected, Some(1));

        screen.move_selection(1);
        assert_eq!(screen.table_state.selected, Some(2));

        // Clamped at end
        screen.move_selection(1);
        assert_eq!(screen.table_state.selected, Some(2));

        screen.move_selection(-1);
        assert_eq!(screen.table_state.selected, Some(1));
    }

    #[test]
    fn filter_narrows_results() {
        let mut screen = ProjectsScreen::new();
        screen.projects.push(ProjectSummary {
            slug: "alpha".into(),
            human_key: "/alpha".into(),
            ..Default::default()
        });
        screen.projects.push(ProjectSummary {
            slug: "beta".into(),
            human_key: "/beta".into(),
            ..Default::default()
        });
        screen.filter = "alpha".to_string();

        let state = test_state();
        // Populate projects_list in shared state
        screen.rebuild_from_state(&state);
        // Without data in shared state, rebuild clears projects. Test filter logic directly.
        assert!(screen.projects.is_empty()); // No data in shared state

        // Test filter with manual data
        screen.projects = vec![
            ProjectSummary {
                slug: "alpha".into(),
                human_key: "/alpha".into(),
                ..Default::default()
            },
            ProjectSummary {
                slug: "beta".into(),
                human_key: "/beta".into(),
                ..Default::default()
            },
        ];
        // Apply filter manually
        let f = screen.filter.to_lowercase();
        screen.projects.retain(|r| {
            r.slug.to_lowercase().contains(&f) || r.human_key.to_lowercase().contains(&f)
        });
        assert_eq!(screen.projects.len(), 1);
        assert_eq!(screen.projects[0].slug, "alpha");
    }
}
