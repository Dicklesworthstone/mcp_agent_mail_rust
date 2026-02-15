//! Reservations screen — active file reservations with TTL progress bars.

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
use ftui_extras::text_effects::{StyledText, TextEffect};
use ftui_runtime::program::Cmd;
use ftui_widgets::progress::ProgressBar;

use crate::tui_action_menu::{ActionEntry, reservations_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

const COL_AGENT: usize = 0;
const COL_PATH: usize = 1;
const COL_EXCLUSIVE: usize = 2;
const COL_TTL: usize = 3;
const COL_PROJECT: usize = 4;

const SORT_LABELS: &[&str] = &["Agent", "Path", "Excl", "TTL", "Project"];

/// Tracked reservation state from events.
#[derive(Debug, Clone)]
struct ActiveReservation {
    agent: String,
    path_pattern: String,
    exclusive: bool,
    granted_ts: i64,
    ttl_s: u64,
    project: String,
    released: bool,
}

#[derive(Debug, Clone)]
struct TtlOverlayRow {
    ratio: f64,
    label: String,
    selected: bool,
    released: bool,
}

impl ActiveReservation {
    /// Remaining seconds until expiry, capped at 0.
    #[allow(clippy::cast_sign_loss)]
    fn remaining_secs(&self) -> u64 {
        let now = chrono::Utc::now().timestamp_micros();
        let expires_micros = self.granted_ts.saturating_add(
            i64::try_from(self.ttl_s)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000),
        );
        let remaining = (expires_micros - now) / 1_000_000;
        if remaining < 0 { 0 } else { remaining as u64 }
    }

    /// Progress ratio (1.0 = full TTL remaining, 0.0 = expired).
    #[allow(clippy::cast_precision_loss)]
    fn ttl_ratio(&self) -> f64 {
        if self.ttl_s == 0 {
            return 0.0;
        }
        let remaining = self.remaining_secs();
        (remaining as f64 / self.ttl_s as f64).clamp(0.0, 1.0)
    }

    /// Composite key for dedup.
    fn key(&self) -> String {
        format!("{}:{}:{}", self.project, self.agent, self.path_pattern)
    }
}

pub struct ReservationsScreen {
    table_state: TableState,
    /// All tracked reservations keyed by composite key.
    reservations: HashMap<String, ActiveReservation>,
    /// Sorted display order (keys into `reservations`).
    sorted_keys: Vec<String>,
    sort_col: usize,
    sort_asc: bool,
    show_released: bool,
    last_seq: u64,
    /// Synthetic event for the focused reservation (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
}

impl ReservationsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            reservations: HashMap::new(),
            sorted_keys: Vec::new(),
            sort_col: COL_TTL,
            sort_asc: true,
            show_released: false,
            last_seq: 0,
            focused_synthetic: None,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected reservation.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self
            .table_state
            .selected
            .and_then(|i| self.sorted_keys.get(i))
            .and_then(|key| self.reservations.get(key))
            .map(|r| {
                crate::tui_events::MailEvent::reservation_granted(
                    &r.agent,
                    vec![r.path_pattern.clone()],
                    r.exclusive,
                    r.ttl_s,
                    &r.project,
                )
            });
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.events_since(self.last_seq);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            match event {
                MailEvent::ReservationGranted {
                    agent,
                    paths,
                    exclusive,
                    ttl_s,
                    project,
                    timestamp_micros,
                    ..
                } => {
                    for path in paths {
                        let res = ActiveReservation {
                            agent: agent.clone(),
                            path_pattern: path.clone(),
                            exclusive: *exclusive,
                            granted_ts: *timestamp_micros,
                            ttl_s: *ttl_s,
                            project: project.clone(),
                            released: false,
                        };
                        self.reservations.insert(res.key(), res);
                    }
                }
                MailEvent::ReservationReleased {
                    agent,
                    paths,
                    project,
                    ..
                } => {
                    for path in paths {
                        let key = format!("{project}:{agent}:{path}");
                        if let Some(res) = self.reservations.get_mut(&key) {
                            res.released = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn rebuild_sorted(&mut self) {
        let show_released = self.show_released;
        let mut entries: Vec<(&String, &ActiveReservation)> = self
            .reservations
            .iter()
            .filter(|(_, r)| show_released || !r.released)
            .collect();

        entries.sort_by(|(_, a), (_, b)| {
            let cmp = match self.sort_col {
                COL_AGENT => a.agent.to_lowercase().cmp(&b.agent.to_lowercase()),
                COL_PATH => a.path_pattern.cmp(&b.path_pattern),
                COL_EXCLUSIVE => a.exclusive.cmp(&b.exclusive),
                COL_TTL => a.remaining_secs().cmp(&b.remaining_secs()),
                COL_PROJECT => a.project.to_lowercase().cmp(&b.project.to_lowercase()),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });

        self.sorted_keys = entries.iter().map(|(k, _)| (*k).clone()).collect();

        // Clamp selection
        if let Some(sel) = self.table_state.selected {
            if sel >= self.sorted_keys.len() {
                self.table_state.selected = if self.sorted_keys.is_empty() {
                    None
                } else {
                    Some(self.sorted_keys.len() - 1)
                };
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sorted_keys.is_empty() {
            return;
        }
        let len = self.sorted_keys.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
    }

    fn summary_counts(&self) -> (usize, usize, usize, usize) {
        let mut active = 0usize;
        let mut exclusive = 0usize;
        let mut shared = 0usize;
        let mut expired = 0usize;
        for res in self.reservations.values() {
            if !res.released {
                active += 1;
                if res.remaining_secs() == 0 {
                    expired += 1;
                }
                if res.exclusive {
                    exclusive += 1;
                } else {
                    shared += 1;
                }
            }
        }
        (active, exclusive, shared, expired)
    }
}

impl Default for ReservationsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ReservationsScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                    KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                    KeyCode::Char('G') | KeyCode::End => {
                        if !self.sorted_keys.is_empty() {
                            self.table_state.selected = Some(self.sorted_keys.len() - 1);
                        }
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        if !self.sorted_keys.is_empty() {
                            self.table_state.selected = Some(0);
                        }
                    }
                    KeyCode::Char('s') => {
                        self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                        self.rebuild_sorted();
                    }
                    KeyCode::Char('S') => {
                        self.sort_asc = !self.sort_asc;
                        self.rebuild_sorted();
                    }
                    KeyCode::Char('x') => {
                        self.show_released = !self.show_released;
                        self.rebuild_sorted();
                    }
                    _ => {}
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.ingest_events(state);
        if tick_count % 10 == 0 {
            self.rebuild_sorted();
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let selected_idx = self.table_state.selected?;
        let key = self.sorted_keys.get(selected_idx)?;
        let reservation = self.reservations.get(key)?;

        // Get actions for this reservation (reservation_id is not available,
        // so we use the path pattern as a pseudo-id)
        #[allow(clippy::cast_possible_wrap)]
        let actions = reservations_actions(
            selected_idx as i64, // Use index as pseudo-id for now
            &reservation.agent,
            &reservation.path_pattern,
        );

        // Anchor row is the selected row + header offset
        #[allow(clippy::cast_possible_truncation)]
        let anchor_row = (selected_idx as u16).saturating_add(2);
        let context_id = key.clone();

        Some((actions, anchor_row, context_id))
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            return;
        }
        let tp = crate::tui_theme::TuiThemePalette::current();
        let effects_enabled = state.config_snapshot().tui_effects;
        let animation_time = state.uptime().as_secs_f64();

        let header_h = 1_u16;
        let table_h = area.height.saturating_sub(header_h);
        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);

        // Summary line
        let (active, exclusive, shared, expired) = self.summary_counts();
        let sort_indicator = if self.sort_asc {
            "\u{25b2}"
        } else {
            "\u{25bc}"
        };
        let sort_label = SORT_LABELS.get(self.sort_col).unwrap_or(&"?");
        let released_label = if self.show_released {
            " [x:show released]"
        } else {
            ""
        };
        let summary_base = format!(
            " {active} active  {exclusive} exclusive  {shared} shared   Sort: {sort_label}{sort_indicator} {released_label}",
        );
        let critical_alert = if expired > 0 {
            format!("  CRITICAL: {expired} expired")
        } else {
            String::new()
        };
        let summary = format!("{summary_base}{critical_alert}");
        let p = Paragraph::new(summary);
        p.render(header_area, frame);
        if !critical_alert.is_empty() {
            let start_offset = u16::try_from(summary_base.len()).unwrap_or(u16::MAX);
            if start_offset < header_area.width {
                let alert_area = Rect::new(
                    header_area.x.saturating_add(start_offset),
                    header_area.y,
                    header_area.width.saturating_sub(start_offset),
                    1,
                );
                if effects_enabled {
                    StyledText::new(critical_alert.trim_start())
                        .effect(TextEffect::PulsingGlow {
                            color: tp.severity_critical,
                            speed: 0.5,
                        })
                        .base_color(tp.severity_critical)
                        .bold()
                        .time(animation_time)
                        .render(alert_area, frame);
                } else {
                    Paragraph::new(critical_alert.trim_start())
                        .style(crate::tui_theme::text_critical(&tp))
                        .render(alert_area, frame);
                }
            }
        }

        // Table rows
        let header = Row::new(["Agent", "Path Pattern", "Excl", "TTL Remaining", "Project"])
            .style(Style::default().bold());

        let mut ttl_overlay_rows: Vec<TtlOverlayRow> = Vec::new();
        let rows: Vec<Row> = self
            .sorted_keys
            .iter()
            .enumerate()
            .filter_map(|(i, key)| {
                let res = self.reservations.get(key)?;
                let excl_str = if res.exclusive {
                    "\u{2713}"
                } else {
                    "\u{2717}"
                };
                let remaining = res.remaining_secs();
                let ratio = res.ttl_ratio();
                let ttl_text = format_ttl(remaining);

                ttl_overlay_rows.push(TtlOverlayRow {
                    ratio,
                    label: ttl_text.clone(),
                    selected: Some(i) == self.table_state.selected,
                    released: res.released,
                });

                let style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else if res.released {
                    crate::tui_theme::text_disabled(&tp)
                } else if remaining == 0 {
                    crate::tui_theme::text_error(&tp)
                } else if ratio < 0.2 {
                    Style::default().fg(tp.ttl_warning)
                } else {
                    Style::default()
                };

                Some(
                    Row::new([
                        res.agent.clone(),
                        res.path_pattern.clone(),
                        excl_str.to_string(),
                        ttl_text,
                        res.project.clone(),
                    ])
                    .style(style),
                )
            })
            .collect();

        let widths = [
            Constraint::Percentage(18.0),
            Constraint::Percentage(27.0),
            Constraint::Percentage(8.0),
            Constraint::Percentage(30.0),
            Constraint::Percentage(17.0),
        ];

        let block = Block::default()
            .title("Reservations")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
        render_ttl_overlays(frame, table_area, &ttl_overlay_rows, &tp);
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate reservations",
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
                key: "x",
                action: "Toggle show released",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("File reservations held by agents. Force-release stale locks with f.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ReservationByAgent(agent) = target {
            // Find the first reservation for this agent and select it
            if let Some(pos) = self.sorted_keys.iter().position(|key| {
                self.reservations
                    .get(key)
                    .is_some_and(|r| r.agent == *agent)
            }) {
                self.table_state.selected = Some(pos);
                return true;
            }
        }
        false
    }

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let key = self.sorted_keys.get(idx)?;
        let res = self.reservations.get(key)?;
        Some(format!("{} ({})", res.path_pattern, res.agent))
    }

    fn title(&self) -> &'static str {
        "Reservations"
    }

    fn tab_label(&self) -> &'static str {
        "Reserv"
    }
}

const fn compute_table_widths(total_width: u16) -> [u16; 5] {
    let c0 = total_width.saturating_mul(18) / 100;
    let c1 = total_width.saturating_mul(27) / 100;
    let c2 = total_width.saturating_mul(8) / 100;
    let c3 = total_width.saturating_mul(30) / 100;
    let used = c0.saturating_add(c1).saturating_add(c2).saturating_add(c3);
    let c4 = total_width.saturating_sub(used);
    [c0, c1, c2, c3, c4]
}

fn ttl_fill_color(
    ratio: f64,
    released: bool,
    tp: &crate::tui_theme::TuiThemePalette,
) -> PackedRgba {
    if released {
        tp.ttl_expired
    } else if ratio < 0.2 {
        tp.ttl_danger
    } else if ratio < 0.5 {
        tp.ttl_warning
    } else {
        tp.ttl_healthy
    }
}

fn render_ttl_overlays(
    frame: &mut Frame<'_>,
    table_area: Rect,
    rows: &[TtlOverlayRow],
    tp: &crate::tui_theme::TuiThemePalette,
) {
    if rows.is_empty() || table_area.width < 8 || table_area.height < 4 {
        return;
    }

    let inner = Rect::new(
        table_area.x.saturating_add(1),
        table_area.y.saturating_add(1),
        table_area.width.saturating_sub(2),
        table_area.height.saturating_sub(2),
    );
    if inner.width < 5 || inner.height < 2 {
        return;
    }

    let widths = compute_table_widths(inner.width);
    let ttl_x = inner
        .x
        .saturating_add(widths[COL_AGENT])
        .saturating_add(widths[COL_PATH])
        .saturating_add(widths[COL_EXCLUSIVE]);
    let ttl_width = widths[COL_TTL];
    if ttl_width < 4 {
        return;
    }

    let first_row_y = inner.y.saturating_add(1);
    let max_visible = usize::from(inner.height.saturating_sub(1));
    for (idx, row) in rows.iter().take(max_visible).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let y = first_row_y.saturating_add(idx as u16);
        if y >= inner.bottom() {
            break;
        }

        let base_style = if row.selected {
            Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
        } else if row.released {
            crate::tui_theme::text_disabled(tp).bg(tp.bg_deep)
        } else {
            crate::tui_theme::text_primary(tp).bg(tp.bg_surface)
        };
        let gauge_bg = if row.selected {
            tp.status_accent
        } else {
            ttl_fill_color(row.ratio, row.released, tp)
        };

        let mut gauge = ProgressBar::new()
            .ratio(row.ratio)
            .style(base_style)
            .gauge_style(Style::default().bg(gauge_bg));
        if ttl_width >= 12 {
            gauge = gauge.label(&row.label);
        }
        gauge.render(Rect::new(ttl_x, y, ttl_width, 1), frame);
    }
}

/// Format remaining seconds as a human-readable string.
fn format_ttl(secs: u64) -> String {
    if secs == 0 {
        return "expired".to_string();
    }
    if secs < 60 {
        format!("{secs}s left")
    } else if secs < 3600 {
        format!("{}m left", secs / 60)
    } else {
        format!("{}h left", secs / 3600)
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
    fn new_screen_defaults() {
        let screen = ReservationsScreen::new();
        assert!(screen.reservations.is_empty());
        assert!(!screen.show_released);
        assert_eq!(screen.sort_col, COL_TTL);
        assert!(screen.sort_asc);
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = ReservationsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = ReservationsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(30, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 30, 3), &state);
    }

    #[test]
    fn renders_tiny_without_panic() {
        let state = test_state();
        let screen = ReservationsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = ReservationsScreen::new();
        assert_eq!(screen.title(), "Reservations");
        assert_eq!(screen.tab_label(), "Reserv");
    }

    #[test]
    fn keybindings_documented() {
        let screen = ReservationsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 3);
        assert!(bindings.iter().any(|b| b.key == "x"));
    }

    #[test]
    fn x_toggles_show_released() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        assert!(!screen.show_released);
        let x = Event::Key(ftui::KeyEvent::new(KeyCode::Char('x')));
        screen.update(&x, &state);
        assert!(screen.show_released);
        screen.update(&x, &state);
        assert!(!screen.show_released);
    }

    #[test]
    fn s_cycles_sort_column() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        let initial = screen.sort_col;
        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn ingest_reservation_events() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            true,
            3600,
            "proj",
        ));
        let _ = state.push_event(MailEvent::reservation_granted(
            "RedStone",
            vec!["tests/*.rs".to_string()],
            false,
            1800,
            "proj",
        ));

        screen.ingest_events(&state);
        assert_eq!(screen.reservations.len(), 2);

        let (active, excl, shared, expired) = screen.summary_counts();
        assert_eq!(active, 2);
        assert_eq!(excl, 1);
        assert_eq!(shared, 1);
        assert_eq!(expired, 0);
    }

    #[test]
    fn ingest_release_events() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            true,
            3600,
            "proj",
        ));
        let _ = state.push_event(MailEvent::reservation_released(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            "proj",
        ));

        screen.ingest_events(&state);
        let (active, _, _, expired) = screen.summary_counts();
        assert_eq!(active, 0);
        assert_eq!(expired, 0);

        // Without show_released, sorted_keys should be empty
        screen.rebuild_sorted();
        assert!(screen.sorted_keys.is_empty());

        // With show_released
        screen.show_released = true;
        screen.rebuild_sorted();
        assert_eq!(screen.sorted_keys.len(), 1);
    }

    #[test]
    fn table_widths_cover_full_inner_width() {
        let widths = compute_table_widths(97);
        assert_eq!(widths.iter().copied().sum::<u16>(), 97);
        assert_eq!(widths[COL_TTL], 29);
    }

    #[test]
    fn ttl_fill_color_thresholds() {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let healthy = ttl_fill_color(0.8, false, &tp);
        assert!(
            healthy.r() > 0 || healthy.g() > 0 || healthy.b() > 0,
            "healthy color should be non-zero"
        );
        let warning = ttl_fill_color(0.3, false, &tp);
        assert!(
            warning.r() > 0 || warning.g() > 0 || warning.b() > 0,
            "warning color should be non-zero"
        );
        let danger = ttl_fill_color(0.1, false, &tp);
        assert!(
            danger.r() > 0 || danger.g() > 0 || danger.b() > 0,
            "danger color should be non-zero"
        );
        let expired = ttl_fill_color(0.8, true, &tp);
        assert!(
            expired.r() > 0 || expired.g() > 0 || expired.b() > 0,
            "expired color should be non-zero"
        );
        // Ensure different bands produce different colors
        assert_ne!(healthy, danger, "healthy and danger should differ");
    }

    #[test]
    fn format_ttl_values() {
        assert_eq!(format_ttl(0), "expired");
        assert_eq!(format_ttl(30), "30s left");
        assert_eq!(format_ttl(300), "5m left");
        assert_eq!(format_ttl(7200), "2h left");
    }

    #[test]
    fn summary_counts_tracks_expired_entries() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            true,
            0,
            "proj",
        ));
        let _ = state.push_event(MailEvent::reservation_granted(
            "RedStone",
            vec!["tests/*.rs".to_string()],
            false,
            1800,
            "proj",
        ));
        screen.ingest_events(&state);
        let (active, exclusive, shared, expired) = screen.summary_counts();
        assert_eq!(active, 2);
        assert_eq!(exclusive, 1);
        assert_eq!(shared, 1);
        assert_eq!(expired, 1);
    }

    #[test]
    fn default_impl() {
        let screen = ReservationsScreen::default();
        assert!(screen.reservations.is_empty());
    }

    #[test]
    fn deep_link_reservation_by_agent() {
        use crate::tui_screens::DeepLinkTarget;

        let state = test_state();
        let mut screen = ReservationsScreen::new();

        // Add some reservations
        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            true,
            3600,
            "proj",
        ));
        let _ = state.push_event(MailEvent::reservation_granted(
            "RedStone",
            vec!["tests/*.rs".to_string()],
            false,
            1800,
            "proj",
        ));

        screen.ingest_events(&state);
        screen.rebuild_sorted();

        // Deep-link to RedStone's reservation
        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ReservationByAgent("RedStone".into()));
        assert!(handled);
        assert!(screen.table_state.selected.is_some());

        // Deep-link to unknown agent
        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ReservationByAgent("Unknown".into()));
        assert!(!handled);
    }
}
