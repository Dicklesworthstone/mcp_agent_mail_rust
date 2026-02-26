//! Projects screen — sortable/filterable project browser with per-project stats,
//! summary band, activity color-coding, responsive columns, and footer summary.

use std::collections::HashMap;

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
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
    /// Previous totals for `MetricTrend` computation.
    prev_totals: (u64, u64, u64, u64),
    /// Whether the detail panel is visible on wide screens.
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
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
            detail_visible: true,
            detail_scroll: 0,
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
        if let Some(sel) = self.table_state.selected
            && sel >= self.projects.len()
        {
            self.table_state.selected = if self.projects.is_empty() {
                None
            } else {
                Some(self.projects.len() - 1)
            };
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
        self.detail_scroll = 0;
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
        // Rebuild every second
        if tick_count.is_multiple_of(10) {
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

        // Outer bordered panel
        let outer_block = crate::tui_panel_helpers::panel_block(" Projects ");
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

        if split.rects.len() >= 2 && self.detail_visible {
            self.render_detail_panel(frame, split.rects[1]);
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
        Some("Project registry with agent counts and message totals.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ProjectBySlug(slug) = target
            && let Some(pos) = self.projects.iter().position(|p| p.slug == *slug)
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
    /// Render summary band + header + table + footer into a single column area.
    #[allow(clippy::cast_possible_truncation)]
    fn render_table_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        tp: &crate::tui_theme::TuiThemePalette,
    ) {
        let wide = area.width >= 120;
        let narrow = area.width < 80;

        let summary_h: u16 = if area.height >= 8 { 2 } else { 0 };
        let header_h: u16 = 1;
        let footer_h = u16::from(area.height >= 6);
        let table_h = area
            .height
            .saturating_sub(summary_h)
            .saturating_sub(header_h)
            .saturating_sub(footer_h);

        let mut y = area.y;

        if summary_h > 0 {
            let summary_area = Rect::new(area.x, y, area.width, summary_h);
            self.render_summary_band(frame, summary_area);
            y += summary_h;
        }

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
            "{} projects   Sort: {}{} {}",
            self.projects.len(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        Paragraph::new(info).render(header_area, frame);

        let table_area = Rect::new(area.x, y, area.width, table_h);
        y += table_h;

        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
            .render(table_area, frame);

        self.render_table(frame, table_area, wide, narrow);

        if footer_h > 0 {
            let footer_area = Rect::new(area.x, y, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }
    }

    /// Render the detail panel for the currently selected project.
    fn render_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Project Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(selected_idx) = self.table_state.selected else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f4c1}",
                "No Project Selected",
                "Select a project from the table to view details.",
            );
            return;
        };

        let Some(proj) = self.projects.get(selected_idx) else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f4c1}",
                "No Project Selected",
                "Select a project from the table to view details.",
            );
            return;
        };

        let mut lines: Vec<(String, String, Option<ftui::PackedRgba>)> = Vec::new();

        let activity_color = self.activity_color(proj);
        let icon = self.activity_icon(proj);

        lines.push(("Slug".into(), proj.slug.clone(), None));
        lines.push(("Path".into(), proj.human_key.clone(), None));
        lines.push((
            "Status".into(),
            format!("{icon} {}",
                if icon == "\u{25CF}" { "Active" }
                else if icon == "\u{25D0}" { "Idle" }
                else { "Inactive" }
            ),
            Some(activity_color),
        ));
        lines.push(("Agents".into(), proj.agent_count.to_string(), Some(tp.metric_agents)));
        lines.push(("Messages".into(), proj.message_count.to_string(), Some(tp.metric_messages)));
        lines.push(("Reservations".into(), proj.reservation_count.to_string(), Some(tp.metric_reservations)));
        lines.push(("Created".into(), format_created_time(proj.created_at), None));

        if let Some(last_ts) = self.project_activity.get(&proj.slug) {
            let relative = format_relative_time(*last_ts);
            lines.push(("Last Activity".into(), relative, None));
        }

        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }

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

/// Render key-value lines with a label column and a value column, supporting scroll.
#[allow(clippy::cast_possible_truncation)]
fn render_kv_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: &[(String, String, Option<ftui::PackedRgba>)],
    scroll: usize,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    use ftui::widgets::Widget;

    let visible_height = usize::from(inner.height);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = scroll.min(max_scroll);
    let label_w = 14u16;

    for (i, (label, value, color)) in lines.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let label_area = Rect::new(inner.x, y, label_w.min(inner.width), 1);
        let label_text = format!("{label}:");
        Paragraph::new(label_text)
            .style(Style::default().fg(tp.text_muted).bold())
            .render(label_area, frame);

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
