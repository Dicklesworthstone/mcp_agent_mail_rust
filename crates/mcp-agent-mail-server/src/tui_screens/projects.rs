//! Projects screen — sortable/filterable project browser with per-project stats,
//! summary band, activity color-coding, responsive columns, and footer summary.

use std::collections::HashMap;

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::text::display_width;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, Style};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{MailEvent, ProjectSummary};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::fancy::SummaryFooter;
use crate::tui_widgets::{MetricTile, MetricTrend};

/// Column indices for sorting.
const COL_SLUG: usize = 0;
const COL_HUMAN_KEY: usize = 1;
const COL_AGENTS: usize = 2;
const COL_MESSAGES: usize = 3;
const COL_RESERVATIONS: usize = 4;
const COL_CREATED: usize = 5;

const SORT_LABELS: &[&str] = &["Slug", "Path", "Agents", "Msgs", "Reserv", "Created"];

/// Activity recency thresholds (microseconds).
const ACTIVE_WINDOW_MICROS: i64 = 5 * 60 * 1_000_000;
const IDLE_WINDOW_MICROS: i64 = 30 * 60 * 1_000_000;

pub struct ProjectsScreen {
    table_state: TableState,
    projects: Vec<ProjectSummary>,
    sort_col: usize,
    sort_asc: bool,
    filter: String,
    filter_active: bool,
    /// Event tracking sequence number.
    last_seq: u64,
    /// Per-project last activity timestamp (slug → micros).
    project_activity: HashMap<String, i64>,
    /// Previous totals for MetricTrend computation.
    prev_totals: (u64, u64, u64, u64),
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
            last_seq: 0,
            project_activity: HashMap::new(),
            prev_totals: (0, 0, 0, 0),
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

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.events_since(self.last_seq);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            if let MailEvent::MessageSent {
                project,
                timestamp_micros,
                ..
            } = event
            {
                self.project_activity
                    .insert(project.clone(), *timestamp_micros);
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

    /// Compute totals for the summary band.
    fn compute_totals(&self) -> (u64, u64, u64, u64) {
        let project_count = self.projects.len() as u64;
        let total_agents: u64 = self.projects.iter().map(|p| p.agent_count).sum();
        let total_msgs: u64 = self.projects.iter().map(|p| p.message_count).sum();
        let total_reserv: u64 = self.projects.iter().map(|p| p.reservation_count).sum();
        (project_count, total_agents, total_msgs, total_reserv)
    }

    /// Determine activity status for a project row.
    fn activity_color(&self, proj: &ProjectSummary) -> ftui::PackedRgba {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let now = chrono::Utc::now().timestamp_micros();
        let last_ts = self.project_activity.get(&proj.slug).copied().unwrap_or(0);

        if last_ts <= 0 {
            return tp.activity_stale;
        }
        let elapsed = now.saturating_sub(last_ts);
        if elapsed <= ACTIVE_WINDOW_MICROS {
            tp.activity_active
        } else if elapsed <= IDLE_WINDOW_MICROS {
            tp.activity_idle
        } else {
            tp.activity_stale
        }
    }

    /// Activity status icon for a project.
    fn activity_icon(&self, proj: &ProjectSummary) -> &'static str {
        let now = chrono::Utc::now().timestamp_micros();
        let last_ts = self.project_activity.get(&proj.slug).copied().unwrap_or(0);

        if last_ts <= 0 {
            return "\u{25CB}"; // ○
        }
        let elapsed = now.saturating_sub(last_ts);
        if elapsed <= ACTIVE_WINDOW_MICROS {
            "\u{25CF}" // ●
        } else if elapsed <= IDLE_WINDOW_MICROS {
            "\u{25D0}" // ◐
        } else {
            "\u{25CB}" // ○
        }
    }
}

impl Default for ProjectsScreen {
    fn default() -> Self {
        Self::new()
    }
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
        self.ingest_events(state);
        // Rebuild every second
        if tick_count % 10 == 0 {
            // Save previous totals for trend computation
            self.prev_totals = self.compute_totals();
            self.rebuild_from_state(state);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let wide = area.width >= 120;
        let narrow = area.width < 80;

        // Layout: summary_band(2) + header(1) + table(remainder) + footer(1)
        let summary_h: u16 = if area.height >= 8 { 2 } else { 0 };
        let header_h: u16 = 1;
        let footer_h: u16 = if area.height >= 6 { 1 } else { 0 };
        let table_h = area
            .height
            .saturating_sub(summary_h)
            .saturating_sub(header_h)
            .saturating_sub(footer_h);

        let mut y = area.y;

        // ── Summary band (MetricTile row) ──────────────────────────────
        if summary_h > 0 {
            let summary_area = Rect::new(area.x, y, area.width, summary_h);
            self.render_summary_band(frame, summary_area);
            y += summary_h;
        }

        // ── Info header ────────────────────────────────────────────────
        let header_area = Rect::new(area.x, y, area.width, header_h);
        y += header_h;

        // Clear header area
        Paragraph::new("")
            .style(Style::default().bg(tp.panel_bg))
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
            "{} projects   Sort: {}{} {}",
            self.projects.len(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        Paragraph::new(info).render(header_area, frame);

        // ── Table ──────────────────────────────────────────────────────
        let table_area = Rect::new(area.x, y, area.width, table_h);
        y += table_h;

        // Clear table region
        Paragraph::new("")
            .style(Style::default().bg(tp.panel_bg))
            .render(table_area, frame);

        self.render_table(frame, table_area, wide, narrow);

        // ── Footer summary ─────────────────────────────────────────────
        if footer_h > 0 {
            let footer_area = Rect::new(area.x, y, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }
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

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let proj = self.projects.get(idx)?;
        Some(proj.human_key.clone())
    }

    fn title(&self) -> &'static str {
        "Projects"
    }

    fn tab_label(&self) -> &'static str {
        "Proj"
    }
}

// ── Rendering helpers ──────────────────────────────────────────────────

impl ProjectsScreen {
    #[allow(clippy::cast_possible_truncation)]
    fn render_summary_band(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let (proj_count, total_agents, total_msgs, total_reserv) = self.compute_totals();
        let (prev_proj, prev_agents, prev_msgs, prev_reserv) = self.prev_totals;

        let proj_str = proj_count.to_string();
        let agents_str = total_agents.to_string();
        let msgs_str = total_msgs.to_string();
        let reserv_str = total_reserv.to_string();

        let tiles: Vec<(&str, &str, MetricTrend, ftui::PackedRgba)> = vec![
            (
                "Projects",
                &proj_str,
                trend_for(proj_count, prev_proj),
                tp.metric_projects,
            ),
            (
                "Agents",
                &agents_str,
                trend_for(total_agents, prev_agents),
                tp.metric_agents,
            ),
            (
                "Messages",
                &msgs_str,
                trend_for(total_msgs, prev_msgs),
                tp.metric_messages,
            ),
            (
                "Reserv",
                &reserv_str,
                trend_for(total_reserv, prev_reserv),
                tp.metric_reservations,
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
            let tile = MetricTile::new(label, value, *trend)
                .value_color(*color)
                .sparkline_color(*color);
            tile.render(tile_area, frame);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_table(&self, frame: &mut Frame<'_>, area: Rect, wide: bool, narrow: bool) {
        let tp = crate::tui_theme::TuiThemePalette::current();

        // Responsive columns
        let (header_cells, widths): (Vec<&str>, Vec<Constraint>) = if narrow {
            // < 80: Slug, Agents, Msgs, Reserv only
            (
                vec!["Slug", "Agents", "Msgs", "Reserv"],
                vec![
                    Constraint::Percentage(40.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                ],
            )
        } else if wide {
            // >= 120: all columns
            (
                vec!["Slug", "Path", "Agents", "Msgs", "Reserv", "Created"],
                vec![
                    Constraint::Percentage(18.0),
                    Constraint::Percentage(30.0),
                    Constraint::Percentage(10.0),
                    Constraint::Percentage(12.0),
                    Constraint::Percentage(10.0),
                    Constraint::Percentage(20.0),
                ],
            )
        } else {
            // 80–119: hide Created
            (
                vec!["Slug", "Path", "Agents", "Msgs", "Reserv"],
                vec![
                    Constraint::Percentage(22.0),
                    Constraint::Percentage(33.0),
                    Constraint::Percentage(12.0),
                    Constraint::Percentage(15.0),
                    Constraint::Percentage(18.0),
                ],
            )
        };

        let header = Row::new(header_cells).style(Style::default().bold());

        let rows: Vec<Row> = self
            .projects
            .iter()
            .enumerate()
            .map(|(i, proj)| {
                let activity_color = self.activity_color(proj);
                let icon = self.activity_icon(proj);
                let style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else {
                    Style::default().fg(activity_color)
                };

                let slug_display = format!("{icon}{}", proj.slug);

                if narrow {
                    Row::new([
                        slug_display,
                        proj.agent_count.to_string(),
                        proj.message_count.to_string(),
                        proj.reservation_count.to_string(),
                    ])
                    .style(style)
                } else if wide {
                    let path_display =
                        truncate_path_front(&proj.human_key, area.width as usize / 4);
                    let created_str = format_created_time(proj.created_at);
                    Row::new([
                        slug_display,
                        path_display,
                        proj.agent_count.to_string(),
                        proj.message_count.to_string(),
                        proj.reservation_count.to_string(),
                        created_str,
                    ])
                    .style(style)
                } else {
                    let path_display =
                        truncate_path_front(&proj.human_key, area.width as usize / 4);
                    Row::new([
                        slug_display,
                        path_display,
                        proj.agent_count.to_string(),
                        proj.message_count.to_string(),
                        proj.reservation_count.to_string(),
                    ])
                    .style(style)
                }
            })
            .collect();

        let block = Block::default()
            .title("Projects")
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
        let (proj_count, total_agents, total_msgs, total_reserv) = self.compute_totals();

        let proj_str = proj_count.to_string();
        let agents_str = total_agents.to_string();
        let msgs_str = total_msgs.to_string();
        let reserv_str = total_reserv.to_string();

        let items: Vec<(&str, &str, ftui::PackedRgba)> = vec![
            (&*proj_str, "projects", tp.metric_projects),
            (&*agents_str, "agents", tp.metric_agents),
            (&*msgs_str, "msgs", tp.metric_messages),
            (&*reserv_str, "reserv", tp.metric_reservations),
        ];

        SummaryFooter::new(&items, tp.text_muted).render(area, frame);
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

/// Truncate a file path for display, preserving the beginning.
///
/// Shows `/home/user/pro…` instead of `...th/here`.
fn truncate_path_front(path: &str, max_len: usize) -> String {
    let w = display_width(path);
    if w <= max_len {
        return path.to_string();
    }
    if max_len < 2 {
        return "\u{2026}".to_string(); // …
    }
    // Keep beginning, add ellipsis
    let mut result = String::new();
    let mut width = 0;
    for ch in path.chars() {
        let cw = display_width(&ch.to_string());
        if width + cw + 1 > max_len {
            break;
        }
        result.push(ch);
        width += cw;
    }
    result.push('\u{2026}'); // …
    result
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
    fn renders_wide_layout() {
        let state = test_state();
        let screen = ProjectsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(140, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 140, 30), &state);
    }

    #[test]
    fn renders_narrow_layout() {
        let state = test_state();
        let screen = ProjectsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);
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
    fn truncate_path_front_values() {
        assert_eq!(truncate_path_front("/short", 20), "/short");
        let truncated = truncate_path_front("/a/very/long/path/here", 10);
        assert!(truncated.starts_with("/a/very/l"));
        assert!(truncated.ends_with('\u{2026}'));
        assert_eq!(truncate_path_front("x", 3), "x");
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
        screen.filter = "alpha".to_string();

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

    #[test]
    fn trend_for_up_down_flat() {
        assert_eq!(trend_for(10, 5), MetricTrend::Up);
        assert_eq!(trend_for(5, 10), MetricTrend::Down);
        assert_eq!(trend_for(5, 5), MetricTrend::Flat);
    }

    #[test]
    fn compute_totals_empty() {
        let screen = ProjectsScreen::new();
        assert_eq!(screen.compute_totals(), (0, 0, 0, 0));
    }

    #[test]
    fn compute_totals_with_data() {
        let mut screen = ProjectsScreen::new();
        screen.projects.push(ProjectSummary {
            agent_count: 3,
            message_count: 10,
            reservation_count: 2,
            ..Default::default()
        });
        screen.projects.push(ProjectSummary {
            agent_count: 5,
            message_count: 20,
            reservation_count: 1,
            ..Default::default()
        });
        assert_eq!(screen.compute_totals(), (2, 8, 30, 3));
    }
}
