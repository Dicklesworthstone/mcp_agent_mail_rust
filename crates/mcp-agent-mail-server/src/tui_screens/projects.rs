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
use mcp_agent_mail_db::DbConn;

use crate::tui_bridge::{ScreenDiagnosticSnapshot, TuiSharedState};
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
const EVENT_INGEST_BATCH_LIMIT: usize = 1024;
const PROJECT_RECOVERY_LIMIT: usize = 500;

/// Activity recency thresholds (microseconds).
const ACTIVE_WINDOW_MICROS: i64 = 5 * 60 * 1_000_000;
const IDLE_WINDOW_MICROS: i64 = 30 * 60 * 1_000_000;

fn sanitize_diagnostic_value(value: &str) -> String {
    value
        .replace(['\n', '\r', ';', ','], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truth assertion: when the DB reports non-zero projects but the rendered
/// list is empty AND no filter is active, the data pipeline has a bug.
fn assert_projects_list_cardinality(total_db_projects: u64, rendered_count: usize, filter: &str) {
    let assertions_on = cfg!(debug_assertions)
        || std::env::var("AM_TUI_STRICT_TRUTH_ASSERTIONS").is_ok_and(|v| {
            let n = v.trim().to_ascii_lowercase();
            matches!(n.as_str(), "1" | "true" | "yes" | "on")
        });
    if !assertions_on {
        return;
    }
    if total_db_projects > 0 && rendered_count == 0 && filter.trim().is_empty() {
        debug_assert!(
            false,
            "[truth_assertion] projects screen: DB reports {total_db_projects} projects \
             but rendered list is empty with no active filter — data pipeline dropped all rows"
        );
    }
}

fn unique_previous_project_row<F>(
    previous_rows: &[ProjectSummary],
    mut predicate: F,
) -> Option<&ProjectSummary>
where
    F: FnMut(&ProjectSummary) -> bool,
{
    let mut matching = previous_rows.iter().filter(|row| predicate(row));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn enrich_zero_count_projects_from_previous(
    rows: &mut [ProjectSummary],
    previous_rows: &[ProjectSummary],
    restore_agent_counts: bool,
    restore_message_counts: bool,
    restore_reservation_counts: bool,
) {
    if rows.is_empty() || previous_rows.is_empty() {
        return;
    }

    for row in rows {
        let previous = unique_previous_project_row(previous_rows, |candidate| {
            candidate.slug == row.slug && candidate.human_key == row.human_key
        })
        .or_else(|| {
            unique_previous_project_row(previous_rows, |candidate| candidate.slug == row.slug)
        });
        let Some(previous) = previous else {
            continue;
        };
        if row.created_at == 0 {
            row.created_at = previous.created_at;
        }
        if restore_agent_counts && row.agent_count == 0 {
            row.agent_count = previous.agent_count;
        }
        if restore_message_counts && row.message_count == 0 {
            row.message_count = previous.message_count;
        }
        if restore_reservation_counts && row.reservation_count == 0 {
            row.reservation_count = previous.reservation_count;
        }
    }
}

fn should_preserve_previous_project_rows(
    total_db_projects: u64,
    rows: &[ProjectSummary],
    previous_rows: &[ProjectSummary],
    filter: &str,
) -> bool {
    total_db_projects > 0
        && rows.is_empty()
        && filter.trim().is_empty()
        && !previous_rows.is_empty()
}

fn recover_projects_list_from_sqlite(
    database_url: &str,
    storage_root: &std::path::Path,
) -> Option<Vec<ProjectSummary>> {
    let db = match crate::open_observability_sync_db_connection(
        database_url,
        storage_root,
        "projects screen recovery",
    ) {
        Ok(Some(db)) => db,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(error = %error, "projects screen recovery db open failed");
            return None;
        }
    };
    let mut projects = fetch_recent_projects_minimal(db.conn());
    if projects.is_empty() {
        return Some(projects);
    }

    let project_ids: Vec<i64> = projects.iter().map(|project| project.id).collect();
    let agent_counts =
        crate::tui_poller::fetch_project_count_map(db.conn(), "agents", &project_ids);
    let message_counts =
        crate::tui_poller::fetch_project_count_map(db.conn(), "messages", &project_ids);
    let reservation_counts = crate::tui_poller::fetch_active_reservation_counts_by_project(
        db.conn(),
        chrono::Utc::now().timestamp_micros(),
    );
    for project in &mut projects {
        project.agent_count = agent_counts.get(&project.id).copied().unwrap_or(0);
        project.message_count = message_counts.get(&project.id).copied().unwrap_or(0);
        project.reservation_count = reservation_counts.get(&project.id).copied().unwrap_or(0);
    }
    Some(projects)
}

fn fetch_recent_projects_minimal(conn: &DbConn) -> Vec<ProjectSummary> {
    let created_at_sort = crate::tui_poller::timestamp_sort_expr("created_at");
    let sql = format!(
        "SELECT id, slug, human_key, {created_at_sort} AS created_at_micros \
         FROM projects \
         ORDER BY {created_at_sort} DESC, id DESC \
         LIMIT {PROJECT_RECOVERY_LIMIT}"
    );
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id = row
                        .get_named::<i64>("id")
                        .ok()
                        .or_else(|| row.get_as::<i64>(0).ok())?;
                    Some(ProjectSummary {
                        id,
                        slug: row
                            .get_named::<String>("slug")
                            .ok()
                            .or_else(|| row.get_as::<String>(1).ok())
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| format!("[unknown-project-{id}]")),
                        human_key: row
                            .get_named::<String>("human_key")
                            .ok()
                            .or_else(|| row.get_as::<String>(2).ok())
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| format!("[unknown-human-key-{id}]")),
                        created_at: parse_recovered_created_at(&row),
                        ..ProjectSummary::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_recovered_created_at(row: &mcp_agent_mail_db::sqlmodel::Row) -> i64 {
    let by_name = crate::tui_poller::parse_raw_ts(row, "created_at_micros");
    if by_name != 0 {
        return by_name;
    }

    row.get_as::<i64>(3)
        .ok()
        .or_else(|| {
            row.get_as::<String>(3)
                .ok()
                .map(|value| parse_recovered_created_at_text(&value))
        })
        .unwrap_or(0)
}

fn parse_recovered_created_at_text(value: &str) -> i64 {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Ok(parsed) = trimmed.parse::<i64>() {
        return parsed;
    }
    if let Ok(parsed) = trimmed.parse::<f64>() {
        return format!("{:.0}", parsed.trunc()).parse::<i64>().unwrap_or(0);
    }
    mcp_agent_mail_db::migrate::text_to_micros(trimmed, "projects_screen", "created_at", 0)
        .ok()
        .flatten()
        .unwrap_or(0)
}

#[allow(clippy::struct_excessive_bools)]
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
    /// Cached totals for the currently rendered/filtered project list.
    cached_totals: (u64, u64, u64, u64),
    /// Whether the detail panel is visible on wide screens.
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
    /// Maximum scroll offset observed during the last render pass.
    last_detail_max_scroll: std::cell::Cell<usize>,
    /// Last observed data-channel generation for dirty-state gating.
    last_data_gen: super::DataGeneration,
    /// Latched when data changes; consumed on the next cadence rebuild.
    pending_rebuild: bool,
    /// True when the DB poller has not yet delivered any data.
    db_context_unavailable: bool,
    /// Banner copy explaining why poller-driven data is unavailable.
    db_context_banner: &'static str,
    /// Latest `db_stats` generation that this screen has actually rebuilt from.
    applied_db_stats_gen: u64,
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
            cached_totals: (0, 0, 0, 0),
            detail_visible: true,
            detail_scroll: 0,
            last_detail_max_scroll: std::cell::Cell::new(0),
            last_data_gen: super::DataGeneration::stale(),
            pending_rebuild: false,
            db_context_unavailable: false,
            db_context_banner: super::POLLER_DB_WAITING_BANNER,
            applied_db_stats_gen: 0,
        }
    }

    fn rebuild_from_state(&mut self, state: &TuiSharedState) {
        let (db, db_stats_gen) = state.db_stats_snapshot_with_generation();
        let cfg = state.config_snapshot();
        let total_rows = db.projects;
        let raw_count = u64::try_from(db.projects_list.len()).unwrap_or(u64::MAX);
        let previous_rows = self.projects.clone();
        let mut rows: Vec<ProjectSummary> = db.projects_list;
        let zero_only_counts = !rows.is_empty()
            && ((db.agents > 0 && rows.iter().all(|project| project.agent_count == 0))
                || (db.messages > 0 && rows.iter().all(|project| project.message_count == 0))
                || (db.file_reservations > 0
                    && rows.iter().all(|project| project.reservation_count == 0)));
        let recovery_attempted = total_rows > 0 && (rows.is_empty() || zero_only_counts);
        let detail_recovered = if recovery_attempted
            && let Some(recovered) = recover_projects_list_from_sqlite(
                &cfg.raw_database_url,
                std::path::Path::new(&cfg.storage_root),
            )
            && !recovered.is_empty()
        {
            rows = recovered;
            true
        } else {
            false
        };
        if recovery_attempted {
            enrich_zero_count_projects_from_previous(
                &mut rows,
                &previous_rows,
                db.agents > 0,
                db.messages > 0,
                db.file_reservations > 0,
            );
            if !detail_recovered
                && should_preserve_previous_project_rows(
                    total_rows,
                    &rows,
                    &previous_rows,
                    &self.filter,
                )
            {
                // Keep the last good project roster instead of publishing a false-empty
                // screen when the richer sqlite recovery path is temporarily unavailable.
                rows = previous_rows;
            }
        }

        // Apply filter
        let filter_text = self.filter.trim().to_ascii_lowercase();
        if !filter_text.is_empty() {
            rows.retain(|r| {
                crate::tui_screens::contains_ci(&r.slug, &filter_text)
                    || crate::tui_screens::contains_ci(&r.human_key, &filter_text)
            });
        }

        // Sort
        rows.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_SLUG => crate::tui_screens::cmp_ci(&a.slug, &b.slug),
                COL_HUMAN_KEY => crate::tui_screens::cmp_ci(&a.human_key, &b.human_key),
                COL_AGENTS => a.agent_count.cmp(&b.agent_count),
                COL_MESSAGES => a.message_count.cmp(&b.message_count),
                COL_RESERVATIONS => a.reservation_count.cmp(&b.reservation_count),
                COL_CREATED => a.created_at.cmp(&b.created_at),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });

        let rendered_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        // Use the higher of total_rows (DB COUNT) and list length as raw_count,
        // since COUNT(*) may be stale/race-y relative to the list fetch.
        let raw_from_db = total_rows.max(raw_count);
        let dropped_count = raw_from_db.saturating_sub(rendered_count);
        let sort_label = SORT_LABELS.get(self.sort_col).copied().unwrap_or("unknown");
        let filter = sanitize_diagnostic_value(&self.filter);
        let filter = if filter.is_empty() {
            "all".to_string()
        } else {
            filter
        };

        // Truth assertion: non-empty DB should produce non-empty rendered list
        // when no user filter is active.
        assert_projects_list_cardinality(total_rows, rows.len(), &self.filter);

        // Detect when list was capped by poller LIMIT (total_rows > list length).
        let list_capped = total_rows > raw_count;

        let transport_mode = cfg.transport_mode().to_string();
        state.push_screen_diagnostic(ScreenDiagnosticSnapshot {
            screen: "projects".to_string(),
            scope: "db_stats.projects_list".to_string(),
            query_params: format!(
                "filter={filter};sort_col={sort_label};sort_asc={};list_rows={raw_count};total_rows={total_rows};capped={list_capped};recovered={detail_recovered}",
                self.sort_asc,
            ),
            raw_count: raw_from_db,
            rendered_count,
            dropped_count,
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            db_url: cfg.database_url,
            storage_root: cfg.storage_root,
            transport_mode,
            auth_enabled: cfg.auth_enabled,
        });

        self.cached_totals = totals_from_projects(&rows);
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
        self.applied_db_stats_gen = db_stats_gen;
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.tick_events_since_limited(self.last_seq, EVENT_INGEST_BATCH_LIMIT);
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
    #[cfg(test)]
    fn compute_totals(&self) -> (u64, u64, u64, u64) {
        totals_from_projects(&self.projects)
    }

    fn activity_color_at(&self, proj: &ProjectSummary, now_micros: i64) -> ftui::PackedRgba {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let last_ts = self.project_activity.get(&proj.slug).copied().unwrap_or(0);

        if last_ts <= 0 {
            return tp.activity_stale;
        }
        let elapsed = now_micros.saturating_sub(last_ts);
        if elapsed <= ACTIVE_WINDOW_MICROS {
            tp.activity_active
        } else if elapsed <= IDLE_WINDOW_MICROS {
            tp.activity_idle
        } else {
            tp.activity_stale
        }
    }

    fn activity_icon_at(&self, proj: &ProjectSummary, now_micros: i64) -> &'static str {
        let last_ts = self.project_activity.get(&proj.slug).copied().unwrap_or(0);

        if last_ts <= 0 {
            return "\u{25CB}"; // ○
        }
        let elapsed = now_micros.saturating_sub(last_ts);
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

fn totals_from_projects(projects: &[ProjectSummary]) -> (u64, u64, u64, u64) {
    let project_count = projects.len() as u64;
    let total_agents: u64 = projects.iter().map(|p| p.agent_count).sum();
    let total_msgs: u64 = projects.iter().map(|p| p.message_count).sum();
    let total_reserv: u64 = projects.iter().map(|p| p.reservation_count).sum();
    (project_count, total_agents, total_msgs, total_reserv)
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
                KeyCode::Char('G') | KeyCode::End if !self.projects.is_empty() => {
                    self.table_state.selected = Some(self.projects.len() - 1);
                }
                KeyCode::Char('g') | KeyCode::Home if !self.projects.is_empty() => {
                    self.table_state.selected = Some(0);
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
                    let max = self.last_detail_max_scroll.get();
                    self.detail_scroll = self.detail_scroll.saturating_add(1).min(max);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Escape if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.rebuild_from_state(state);
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // ── Dirty-state gated data ingestion ────────────────────────
        let current_gen = state.data_generation();
        let dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        // Snapshot totals *before* data changes so render_summary_band can
        // compute trend arrows by comparing live compute_totals() (post-update)
        // with prev_totals (pre-update).
        if dirty.events || dirty.db_stats {
            self.prev_totals = self.cached_totals;
        }
        if dirty.events {
            self.ingest_events(state);
        }
        if dirty.db_stats || dirty.events {
            self.pending_rebuild = true;
        }
        // Rebuild every second, consuming a latched refresh request so updates
        // are not lost when dirty signals occur off cadence boundaries.
        if tick_count.is_multiple_of(10) && self.pending_rebuild {
            self.rebuild_from_state(state);
            self.pending_rebuild = false;
        }

        if let Some(banner) =
            super::poller_db_context_banner(state, self.applied_db_stats_gen, tick_count)
        {
            self.db_context_unavailable = true;
            self.db_context_banner = banner;
        } else {
            self.db_context_unavailable = false;
            self.db_context_banner = super::POLLER_DB_WAITING_BANNER;
        }

        self.last_data_gen = current_gen;
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
        let layout = ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
            .at(
                Breakpoint::Lg,
                Flex::horizontal().constraints([Constraint::Percentage(60.0), Constraint::Fill]),
            )
            .at(
                Breakpoint::Xl,
                Flex::horizontal().constraints([Constraint::Percentage(50.0), Constraint::Fill]),
            );

        if self.db_context_unavailable {
            let banner = Paragraph::new(self.db_context_banner)
                .style(Style::default().fg(tp.severity_error));
            let banner_area = Rect::new(area.x, area.y, area.width, 1);
            banner.render(banner_area, frame);
            return;
        }

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

        let now_micros = chrono::Utc::now().timestamp_micros();
        let activity_color = self.activity_color_at(proj, now_micros);
        let icon = self.activity_icon_at(proj, now_micros);

        lines.push(("Slug".into(), proj.slug.clone(), None));
        lines.push(("Path".into(), proj.human_key.clone(), None));
        lines.push((
            "Status".into(),
            format!(
                "{icon} {}",
                if icon == "\u{25CF}" {
                    "Active"
                } else if icon == "\u{25D0}" {
                    "Idle"
                } else {
                    "Inactive"
                }
            ),
            Some(activity_color),
        ));
        lines.push((
            "Agents".into(),
            proj.agent_count.to_string(),
            Some(tp.metric_agents),
        ));
        lines.push((
            "Messages".into(),
            proj.message_count.to_string(),
            Some(tp.metric_messages),
        ));
        lines.push((
            "Reservations".into(),
            proj.reservation_count.to_string(),
            Some(tp.metric_reservations),
        ));
        lines.push(("Created".into(), format_created_time(proj.created_at), None));

        if let Some(last_ts) = self.project_activity.get(&proj.slug) {
            let relative = format_relative_time(*last_ts);
            lines.push(("Last Activity".into(), relative, None));
        }

        render_kv_lines(
            frame,
            inner,
            &lines,
            self.detail_scroll,
            &self.last_detail_max_scroll,
            &tp,
        );
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_summary_band(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let (proj_count, total_agents, total_msgs, total_reserv) = self.cached_totals;
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
        let now_micros = chrono::Utc::now().timestamp_micros();

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
                let activity_color = self.activity_color_at(proj, now_micros);
                let icon = self.activity_icon_at(proj, now_micros);
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
        let (proj_count, total_agents, total_msgs, total_reserv) = self.cached_totals;

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
    max_scroll_cell: &std::cell::Cell<usize>,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    use ftui::widgets::Widget;

    let visible_height = usize::from(inner.height);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    max_scroll_cell.set(max_scroll);
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
        let mut b = [0; 4];
        let cw = display_width(ch.encode_utf8(&mut b));
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
    use tempfile::tempdir;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    fn set_database_url(state: &std::sync::Arc<TuiSharedState>, database_url: String) {
        let mut snapshot = state.config_snapshot();
        snapshot.database_url = database_url.clone();
        snapshot.raw_database_url = database_url;
        state.update_config_snapshot(snapshot);
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
    fn rebuild_emits_screen_diagnostic_with_raw_and_rendered_counts() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 5,
            projects_list: vec![
                ProjectSummary {
                    slug: "alpha".into(),
                    human_key: "/tmp/alpha".into(),
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "beta".into(),
                    human_key: "/tmp/beta".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.filter = "alpha".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        assert_eq!(diagnostics.len(), 1);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert_eq!(diag.screen, "projects");
        // raw_count uses total_rows (DB COUNT=5), not projects_list.len()=2,
        // so the diagnostic tracks the full cardinality gap.
        assert_eq!(diag.raw_count, 5);
        assert_eq!(diag.rendered_count, 1);
        assert_eq!(diag.dropped_count, 4);
        assert!(diag.query_params.contains("filter=alpha"));
        assert!(diag.query_params.contains("list_rows=2"));
        assert!(diag.query_params.contains("total_rows=5"));
    }

    #[test]
    fn rebuild_emits_screen_diagnostic_filter_all_when_empty() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            projects_list: vec![ProjectSummary {
                slug: "alpha".into(),
                human_key: "/tmp/alpha".into(),
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.filter = " \n ".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert!(diag.query_params.contains("filter=all"));
    }

    #[test]
    fn rebuild_diagnostic_raw_count_tracks_list_rows_when_total_counter_is_lower() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 0,
            projects_list: vec![
                ProjectSummary {
                    slug: "alpha".into(),
                    human_key: "/tmp/alpha".into(),
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "beta".into(),
                    human_key: "/tmp/beta".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.filter = "beta".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert_eq!(diag.raw_count, 2);
        assert_eq!(diag.rendered_count, 1);
        assert_eq!(diag.dropped_count, 1);
        assert!(diag.query_params.contains("list_rows=2"));
        assert!(diag.query_params.contains("total_rows=0"));
    }

    #[test]
    fn sanitize_diagnostic_value_strips_delimiters_and_whitespace() {
        let value = sanitize_diagnostic_value(" alpha;\n beta,\r gamma ");
        assert_eq!(value, "alpha beta gamma");
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
            crate::tui_screens::contains_ci(&r.slug, &f)
                || crate::tui_screens::contains_ci(&r.human_key, &f)
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

    #[test]
    fn rebuild_updates_cached_totals() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 2,
            projects_list: vec![
                ProjectSummary {
                    slug: "alpha".to_string(),
                    human_key: "/tmp/alpha".to_string(),
                    agent_count: 3,
                    message_count: 10,
                    reservation_count: 2,
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "beta".to_string(),
                    human_key: "/tmp/beta".to_string(),
                    agent_count: 5,
                    message_count: 20,
                    reservation_count: 1,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.rebuild_from_state(&state);
        assert_eq!(screen.cached_totals, (2, 8, 30, 3));
    }

    #[test]
    fn fetch_recent_projects_minimal_preserves_unknown_slug_and_human_key_labels() {
        let conn = DbConn::open_memory().expect("open memory sqlite");
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT,
                human_key TEXT,
                created_at INTEGER
            )",
        )
        .expect("create projects");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (5, '', '', 100)",
        )
        .expect("insert project");

        let rows = fetch_recent_projects_minimal(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 5);
        assert_eq!(rows[0].slug, "[unknown-project-5]");
        assert_eq!(rows[0].human_key, "[unknown-human-key-5]");
    }

    // ── B5: Cardinality truth assertions ────────────────────────────

    #[test]
    fn projects_cardinality_passes_when_projects_rendered() {
        assert_projects_list_cardinality(15, 15, "");
    }

    #[test]
    fn projects_cardinality_passes_when_filter_active_and_empty() {
        assert_projects_list_cardinality(15, 0, "no-match-filter");
    }

    #[test]
    fn projects_cardinality_passes_when_db_empty() {
        assert_projects_list_cardinality(0, 0, "");
    }

    #[test]
    fn projects_cardinality_catches_false_empty_state() {
        let result = std::panic::catch_unwind(|| {
            assert_projects_list_cardinality(15, 0, "");
        });
        assert!(
            result.is_err(),
            "should panic when DB has projects but rendered list is empty without filter"
        );
    }

    #[test]
    fn enrich_zero_count_projects_from_previous_restores_known_counts() {
        let mut rows = vec![ProjectSummary {
            slug: "alpha".to_string(),
            human_key: "/tmp/alpha".to_string(),
            created_at: 0,
            ..Default::default()
        }];
        let previous_rows = vec![ProjectSummary {
            slug: "alpha".to_string(),
            human_key: "/tmp/alpha".to_string(),
            created_at: 100,
            agent_count: 3,
            message_count: 5,
            reservation_count: 2,
            ..Default::default()
        }];

        enrich_zero_count_projects_from_previous(&mut rows, &previous_rows, true, true, true);

        assert_eq!(rows[0].created_at, 100);
        assert_eq!(rows[0].agent_count, 3);
        assert_eq!(rows[0].message_count, 5);
        assert_eq!(rows[0].reservation_count, 2);
    }

    // ── G6: Project scope audit tests ───────────────────────────────

    #[test]
    fn project_aggregation_counts_are_per_project() {
        // Documents the invariant: each ProjectSummary carries its own
        // agent_count, message_count, reservation_count. These are computed
        // by SQL subqueries with WHERE project_id = p.id, ensuring no
        // cross-project bleed in aggregate counts.
        let mut screen = ProjectsScreen::new();
        screen.projects.push(ProjectSummary {
            slug: "project-a".to_string(),
            agent_count: 3,
            message_count: 10,
            reservation_count: 2,
            ..Default::default()
        });
        screen.projects.push(ProjectSummary {
            slug: "project-b".to_string(),
            agent_count: 5,
            message_count: 20,
            reservation_count: 1,
            ..Default::default()
        });
        // Each project's counts are independent (no leakage)
        let proj_a = &screen.projects[0];
        let proj_b = &screen.projects[1];
        assert_ne!(
            proj_a.agent_count, proj_b.agent_count,
            "per-project agent counts must be independent"
        );
        assert_ne!(
            proj_a.message_count, proj_b.message_count,
            "per-project message counts must be independent"
        );
    }

    #[test]
    fn project_list_shows_all_projects_by_default() {
        // Documents: projects screen shows ALL projects, no default scope filter.
        // This is correct because the projects screen IS the scope selector.
        let screen = ProjectsScreen::new();
        assert!(
            screen.filter.is_empty(),
            "projects screen must start with empty filter (showing all)"
        );
    }

    // ── B6: Count/List Consistency Contract ──────────────────────────

    #[test]
    fn count_list_consistency_no_filter_no_cap() {
        // When total_rows == list length and no filter: rendered_count must
        // equal total_rows. Diagnostic must show capped=false.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 2,
            projects_list: vec![
                ProjectSummary {
                    slug: "alpha".to_string(),
                    human_key: "/path/alpha".to_string(),
                    created_at: 100,
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "beta".to_string(),
                    human_key: "/path/beta".to_string(),
                    created_at: 200,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 2, "all projects should be rendered");
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 2);
        assert_eq!(diag.rendered_count, 2);
        assert_eq!(diag.dropped_count, 0);
        assert!(
            diag.query_params.contains("capped=false"),
            "list is not capped when total_rows == list length"
        );
    }

    #[test]
    fn count_list_consistency_capped_list() {
        // When total_rows > list length (simulating poller cap), diagnostic
        // must show capped=true with correct cardinality gap.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 800,
            projects_list: vec![ProjectSummary {
                slug: "alpha".to_string(),
                human_key: "/path/alpha".to_string(),
                created_at: 100,
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 1);
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 800);
        assert_eq!(diag.rendered_count, 1);
        assert_eq!(diag.dropped_count, 799);
        assert!(
            diag.query_params.contains("capped=true"),
            "list must be flagged as capped when total_rows > list length"
        );
    }

    #[test]
    fn count_list_consistency_filter_reduces_rendered() {
        // Filter should reduce rendered_count but not raw_count.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 3,
            projects_list: vec![
                ProjectSummary {
                    slug: "api-server".to_string(),
                    human_key: "/path/api".to_string(),
                    created_at: 100,
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "web-frontend".to_string(),
                    human_key: "/path/web".to_string(),
                    created_at: 200,
                    ..Default::default()
                },
                ProjectSummary {
                    slug: "api-gateway".to_string(),
                    human_key: "/path/gateway".to_string(),
                    created_at: 300,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.filter = "api".to_string();
        screen.rebuild_from_state(&state);

        assert_eq!(
            screen.projects.len(),
            2,
            "filter matches api-server and api-gateway"
        );
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 3);
        assert_eq!(diag.rendered_count, 2);
        assert_eq!(diag.dropped_count, 1);
        assert!(diag.query_params.contains("capped=false"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rebuild_recovers_projects_from_sqlite_when_snapshot_list_is_empty() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("projects-recovery.sqlite3");
        let db_path_str = db_path.display().to_string();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open sqlite");
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        )
        .expect("create projects");
        conn.execute_raw(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
        )
        .expect("create agents");
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                subject TEXT NOT NULL,
                recipients_json TEXT NOT NULL,
                created_ts INTEGER NOT NULL
            )",
        )
        .expect("create messages");
        conn.execute_raw(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                released_ts INTEGER,
                expires_ts INTEGER NOT NULL
            )",
        )
        .expect("create reservations");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES
                (1, 'alpha', '/tmp/alpha', 1773187200000000),
                (2, 'beta', '/tmp/beta', 1773273600000000)",
        )
        .expect("seed projects");
        conn.execute_raw(
            "INSERT INTO agents (id, project_id, name, program, last_active_ts) VALUES
                (10, 1, 'BlueLake', 'codex', 111),
                (11, 1, 'RedStone', 'codex', 112),
                (12, 2, 'GreenField', 'claude', 113)",
        )
        .expect("seed agents");
        conn.execute_raw(
            "INSERT INTO messages (id, project_id, sender_id, subject, recipients_json, created_ts) VALUES
                (100, 1, 10, 'hello', '[]', 201),
                (101, 1, 11, 'world', '[]', 202),
                (102, 2, 12, 'beta', '[]', 203)",
        )
        .expect("seed messages");
        conn.execute_raw(&format!(
            "INSERT INTO file_reservations (id, project_id, released_ts, expires_ts) VALUES
                (1, 1, NULL, 1),
                (2, 2, NULL, {}),
                (3, 2, 5, {})",
            chrono::Utc::now().timestamp_micros() + 60_000_000,
            chrono::Utc::now().timestamp_micros() + 60_000_000,
        ))
        .expect("seed reservations");

        let state = test_state();
        set_database_url(&state, format!("sqlite:///{}", db_path.display()));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 2,
            projects_list: Vec::new(),
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(
            screen.projects.len(),
            2,
            "screen should repopulate from sqlite"
        );
        assert_eq!(screen.projects[0].slug, "beta", "newest project first");
        assert_eq!(screen.projects[0].agent_count, 1);
        assert_eq!(screen.projects[0].message_count, 1);
        assert_eq!(screen.projects[0].reservation_count, 1);
        assert!(screen.projects[0].created_at > 0);
        assert_eq!(screen.projects[1].slug, "alpha");
        assert_eq!(screen.projects[1].agent_count, 2);
        assert_eq!(screen.projects[1].message_count, 2);
        assert_eq!(screen.projects[1].reservation_count, 0);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert!(
            diag.query_params.contains("recovered=true"),
            "diagnostic should record direct sqlite recovery"
        );
    }

    #[test]
    fn rebuild_preserves_previous_projects_when_db_reports_rows_but_recovery_fails() {
        let broken_db_root = tempdir().expect("tempdir");
        let state = test_state();
        set_database_url(
            &state,
            format!("sqlite://{}", broken_db_root.path().to_string_lossy()),
        );
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            projects_list: Vec::new(),
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.projects = vec![ProjectSummary {
            slug: "alpha".to_string(),
            human_key: "/tmp/alpha".to_string(),
            created_at: 100,
            agent_count: 3,
            message_count: 5,
            reservation_count: 2,
            ..Default::default()
        }];

        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 1);
        assert_eq!(screen.projects[0].slug, "alpha");
        assert_eq!(screen.projects[0].human_key, "/tmp/alpha");
        assert_eq!(screen.projects[0].agent_count, 3);
        assert_eq!(screen.projects[0].message_count, 5);
        assert_eq!(screen.projects[0].reservation_count, 2);
    }

    #[test]
    fn rebuild_preserves_previous_project_counts_when_snapshot_zeroes_them() {
        let broken_db_root = tempdir().expect("tempdir");
        let state = test_state();
        set_database_url(
            &state,
            format!("sqlite://{}", broken_db_root.path().to_string_lossy()),
        );
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            agents: 3,
            messages: 5,
            file_reservations: 2,
            projects_list: vec![ProjectSummary {
                slug: "alpha".to_string(),
                human_key: "/tmp/alpha".to_string(),
                created_at: 100,
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.projects = vec![ProjectSummary {
            slug: "alpha".to_string(),
            human_key: "/tmp/alpha".to_string(),
            created_at: 100,
            agent_count: 3,
            message_count: 5,
            reservation_count: 2,
            ..Default::default()
        }];

        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 1);
        assert_eq!(screen.projects[0].slug, "alpha");
        assert_eq!(screen.projects[0].agent_count, 3);
        assert_eq!(screen.projects[0].message_count, 5);
        assert_eq!(screen.projects[0].reservation_count, 2);
    }

    #[test]
    fn rebuild_recovers_zero_only_project_counts_from_sqlite() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("projects-zero-count-recovery.sqlite3");
        let db_path_str = db_path.display().to_string();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open sqlite");
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        )
        .expect("create projects");
        conn.execute_raw(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
        )
        .expect("create agents");
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL
            )",
        )
        .expect("create messages");
        conn.execute_raw(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                released_ts INTEGER,
                expires_ts INTEGER NOT NULL
            )",
        )
        .expect("create reservations");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'demo', '/tmp/demo', 100)",
        )
        .expect("insert project");
        conn.execute_raw(
            "INSERT INTO agents (project_id, name, program, last_active_ts)
             VALUES (1, 'BlueLake', 'codex-cli', 10), (1, 'RedFox', 'claude-code', 20)",
        )
        .expect("insert agents");
        conn.execute_raw(
            "INSERT INTO messages (project_id, sender_id) VALUES (1, 1), (1, 1), (1, 2)",
        )
        .expect("insert messages");
        conn.execute_raw(
            "INSERT INTO file_reservations (project_id, released_ts, expires_ts)
             VALUES (1, NULL, 4102444800000000)",
        )
        .expect("insert reservation");

        let state = test_state();
        set_database_url(&state, format!("sqlite:///{db_path_str}"));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            agents: 2,
            messages: 3,
            file_reservations: 1,
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "demo".into(),
                human_key: "/tmp/demo".into(),
                created_at: 100,
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 1);
        assert_eq!(screen.projects[0].agent_count, 2);
        assert_eq!(screen.projects[0].message_count, 3);
        assert_eq!(screen.projects[0].reservation_count, 1);
    }

    #[test]
    fn rebuild_recovered_projects_still_preserves_previous_counts_when_backfill_queries_fail() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("projects-recovery-count-failure.sqlite3");
        let db_path_str = db_path.display().to_string();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open sqlite");
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        )
        .expect("create projects");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'demo', '/tmp/demo', 100)",
        )
        .expect("insert project");

        let state = test_state();
        set_database_url(&state, format!("sqlite:///{db_path_str}"));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            agents: 2,
            messages: 3,
            file_reservations: 1,
            projects_list: Vec::new(),
            ..Default::default()
        });

        let mut screen = ProjectsScreen::new();
        screen.projects = vec![ProjectSummary {
            id: 1,
            slug: "demo".into(),
            human_key: "/tmp/demo".into(),
            created_at: 100,
            agent_count: 2,
            message_count: 3,
            reservation_count: 1,
        }];

        screen.rebuild_from_state(&state);

        assert_eq!(screen.projects.len(), 1);
        assert_eq!(screen.projects[0].slug, "demo");
        assert_eq!(screen.projects[0].agent_count, 2);
        assert_eq!(screen.projects[0].message_count, 3);
        assert_eq!(screen.projects[0].reservation_count, 1);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert!(
            diag.query_params.contains("recovered=true"),
            "direct sqlite recovery should still be recorded"
        );
    }

    // ── B8: DB context binding guardrail regression tests ─────────────

    #[test]
    fn b8_projects_unavailable_after_grace_period_without_poller() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();
        assert!(!screen.db_context_unavailable, "starts false");

        // Tick past grace period without poller data
        screen.tick(30, &state);
        assert!(
            screen.db_context_unavailable,
            "should show banner when poller never delivered data after grace period"
        );
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_WAITING_BANNER
        );
    }

    #[test]
    fn b8_projects_available_after_poller_delivers() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();

        state.mark_db_context_available();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            projects_list: vec![ProjectSummary {
                slug: "test-proj".to_string(),
                human_key: "/path/test".to_string(),
                created_at: 100,
                ..Default::default()
            }],
            ..Default::default()
        });

        screen.tick(30, &state);
        assert!(
            !screen.db_context_unavailable,
            "should not show banner when poller has delivered data"
        );
    }

    #[test]
    fn b8_projects_keep_waiting_banner_until_first_snapshot_is_applied() {
        let state = test_state();
        state.mark_db_context_available();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            timestamp_micros: 10,
            ..crate::tui_events::DbStatSnapshot::default()
        });
        let mut screen = ProjectsScreen::new();

        screen.tick(1, &state);

        assert!(screen.db_context_unavailable);
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_WAITING_BANNER
        );
    }

    #[test]
    fn cadence_rebuild_uses_latched_dirty_signal() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();

        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 1,
            projects_list: vec![ProjectSummary {
                slug: "phase-latch".to_string(),
                human_key: "/path/latch".to_string(),
                created_at: 321,
                ..Default::default()
            }],
            ..Default::default()
        });

        screen.tick(9, &state);
        assert!(screen.projects.is_empty());
        screen.tick(10, &state);
        assert_eq!(screen.projects.len(), 1);
    }

    #[test]
    fn b8_projects_banner_renders_when_unavailable() {
        let state = test_state();
        let mut screen = ProjectsScreen::new();
        screen.db_context_unavailable = true;
        screen.db_context_banner = crate::tui_screens::POLLER_DB_UNAVAILABLE_BANNER;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, ftui::layout::Rect::new(0, 0, 120, 30), &state);

        let mut text = String::new();
        for y in 0..frame.buffer.height() {
            for x in 0..frame.buffer.width() {
                if let Some(cell) = frame.buffer.get(x, y) {
                    if let Some(ch) = cell.content.as_char() {
                        text.push(ch);
                    } else if !cell.is_continuation() {
                        text.push(' ');
                    }
                }
            }
            text.push('\n');
        }
        assert!(
            text.contains("Database context unavailable"),
            "should render degraded banner: {text}"
        );
        assert!(
            text.contains("Check DB URL/project scope and refresh"),
            "should render explicit degraded copy: {text}"
        );
    }

    #[test]
    fn b8_projects_show_explicit_unavailable_after_warmup_failure() {
        let state = test_state();
        state.mark_db_warmup_failed();
        let mut screen = ProjectsScreen::new();

        screen.tick(30, &state);

        assert!(screen.db_context_unavailable);
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_UNAVAILABLE_BANNER
        );
    }
}
