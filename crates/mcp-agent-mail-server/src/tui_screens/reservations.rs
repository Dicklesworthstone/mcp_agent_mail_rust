//! Reservations screen — active file reservations with TTL progress bars.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::text::display_width;
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

use crate::tui_action_menu::{ActionEntry, reservations_actions, reservations_batch_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{DbStatSnapshot, MailEvent, ReservationSnapshot};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg, SelectionState};

const COL_AGENT: usize = 0;
const COL_PATH: usize = 1;
const COL_EXCLUSIVE: usize = 2;
const COL_TTL: usize = 3;
const COL_PROJECT: usize = 4;

const SORT_LABELS: &[&str] = &["Agent", "Path", "Excl", "TTL", "Project"];
/// Number of empty DB snapshots tolerated before pruning active rows.
const EMPTY_SNAPSHOT_HOLD_CYCLES: u8 = 1;
/// Minimum tick spacing between direct DB fallback probes.
const FALLBACK_DB_REFRESH_TICKS: u64 = 10;

/// Tracked reservation state from events.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveReservation {
    reservation_id: Option<i64>,
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
    /// Remaining seconds until expiry at `now_micros`, capped at 0.
    #[allow(clippy::cast_sign_loss)]
    fn remaining_secs_at(&self, now_micros: i64) -> u64 {
        let expires_micros = self.granted_ts.saturating_add(
            i64::try_from(self.ttl_s)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000),
        );
        let remaining = (expires_micros - now_micros) / 1_000_000;
        if remaining < 0 { 0 } else { remaining as u64 }
    }

    /// Remaining seconds until expiry, capped at 0.
    fn remaining_secs(&self) -> u64 {
        self.remaining_secs_at(chrono::Utc::now().timestamp_micros())
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
        reservation_key(&self.project, &self.agent, &self.path_pattern)
    }
}

fn reservation_key(project: &str, agent: &str, path_pattern: &str) -> String {
    format!("{project}:{agent}:{path_pattern}")
}

pub struct ReservationsScreen {
    table_state: TableState,
    /// All tracked reservations keyed by composite key.
    reservations: HashMap<String, ActiveReservation>,
    /// Sorted display order (keys into `reservations`).
    sorted_keys: Vec<String>,
    /// Multi-selection state keyed by reservation composite key.
    selected_reservation_keys: SelectionState<String>,
    sort_col: usize,
    sort_asc: bool,
    show_released: bool,
    last_seq: u64,
    /// Timestamp of the last DB snapshot consumed by this screen.
    last_snapshot_micros: i64,
    /// Consecutive empty DB snapshots seen while active rows exist.
    empty_snapshot_streak: u8,
    /// Synthetic event for the focused reservation (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
    /// Last table scroll offset computed during render.
    last_render_offset: Cell<usize>,
    /// Last rendered table area for mouse hit-testing.
    last_table_area: Cell<Rect>,
    /// Last fallback probe failure details, shown in empty-state diagnostics.
    fallback_issue: Option<String>,
    /// Tick index of the last direct fallback probe.
    last_fallback_probe_tick: u64,
}

impl ReservationsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            reservations: HashMap::new(),
            sorted_keys: Vec::new(),
            selected_reservation_keys: SelectionState::new(),
            sort_col: COL_TTL,
            sort_asc: true,
            show_released: false,
            last_seq: 0,
            last_snapshot_micros: 0,
            empty_snapshot_streak: 0,
            focused_synthetic: None,
            last_render_offset: Cell::new(0),
            last_table_area: Cell::new(Rect::new(0, 0, 0, 0)),
            fallback_issue: None,
            last_fallback_probe_tick: 0,
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

    fn ingest_events(&mut self, state: &TuiSharedState) -> bool {
        let mut changed = false;
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
                            reservation_id: None,
                            agent: agent.clone(),
                            path_pattern: path.clone(),
                            exclusive: *exclusive,
                            granted_ts: *timestamp_micros,
                            ttl_s: *ttl_s,
                            project: project.clone(),
                            released: false,
                        };
                        let key = res.key();
                        if self.reservations.get(&key) != Some(&res) {
                            self.reservations.insert(key, res);
                            changed = true;
                        }
                    }
                }
                MailEvent::ReservationReleased {
                    agent,
                    paths,
                    project,
                    ..
                } => {
                    for token in paths {
                        changed |= self.mark_released(project, agent, token);
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn mark_released(&mut self, project: &str, agent: &str, token: &str) -> bool {
        if token == "<all-active>" {
            let mut changed = false;
            for res in self.reservations.values_mut() {
                if res.project == project && res.agent == agent && !res.released {
                    res.released = true;
                    changed = true;
                }
            }
            return changed;
        }

        if let Some(id_str) = token.strip_prefix("id:") {
            if let Ok(target_id) = id_str.parse::<i64>() {
                let mut changed = false;
                for res in self.reservations.values_mut() {
                    if res.project == project
                        && res.agent == agent
                        && res.reservation_id == Some(target_id)
                        && !res.released
                    {
                        res.released = true;
                        changed = true;
                    }
                }
                if changed {
                    return true;
                }

                // The event stream does not always carry reservation IDs on
                // grant events. If there is exactly one active candidate for
                // this agent/project, reconcile release eagerly instead of
                // waiting for the next DB snapshot to map `id:*`.
                let mut candidates: Vec<_> = self
                    .reservations
                    .iter_mut()
                    .filter(|(_, res)| {
                        res.project == project && res.agent == agent && !res.released
                    })
                    .collect();
                if candidates.len() == 1 {
                    let (_, res) = candidates.remove(0);
                    res.released = true;
                    res.reservation_id = Some(target_id);
                    return true;
                }
                return changed;
            }
        }

        let key = reservation_key(project, agent, token);
        if let Some(res) = self.reservations.get_mut(&key) {
            if !res.released {
                res.released = true;
                return true;
            }
        }
        false
    }

    fn ttl_secs_from_snapshot(snapshot: &ReservationSnapshot) -> u64 {
        if snapshot.expires_ts <= snapshot.granted_ts {
            return 0;
        }
        let ttl_micros = snapshot.expires_ts.saturating_sub(snapshot.granted_ts);
        let ttl_secs = ttl_micros.saturating_add(999_999) / 1_000_000;
        u64::try_from(ttl_secs).unwrap_or(u64::MAX)
    }

    fn apply_db_snapshot(&mut self, snapshot: &DbStatSnapshot) -> bool {
        if snapshot.timestamp_micros <= self.last_snapshot_micros {
            return false;
        }
        self.last_snapshot_micros = snapshot.timestamp_micros;

        let had_active_before = self.reservations.values().any(|res| !res.released);
        let hold_active_rows = if snapshot.reservation_snapshots.is_empty() && had_active_before {
            let hold = self.empty_snapshot_streak < EMPTY_SNAPSHOT_HOLD_CYCLES;
            self.empty_snapshot_streak = self.empty_snapshot_streak.saturating_add(1);
            hold
        } else {
            self.empty_snapshot_streak = 0;
            false
        };
        let snapshot_truncated = snapshot.file_reservations
            > u64::try_from(snapshot.reservation_snapshots.len()).unwrap_or(u64::MAX);

        let mut seen_active: HashSet<String> = HashSet::new();
        let mut next = self.reservations.clone();
        for row in &snapshot.reservation_snapshots {
            let key = reservation_key(&row.project_slug, &row.agent_name, &row.path_pattern);
            seen_active.insert(key.clone());
            let reservation = ActiveReservation {
                reservation_id: Some(row.id),
                agent: row.agent_name.clone(),
                path_pattern: row.path_pattern.clone(),
                exclusive: row.exclusive,
                granted_ts: row.granted_ts,
                ttl_s: Self::ttl_secs_from_snapshot(row),
                project: row.project_slug.clone(),
                // DB snapshot rows are authoritative. If a previously released
                // row key is re-acquired, clear stale `released` state.
                released: row.is_released(),
            };
            next.insert(key, reservation);
        }
        // Keep released history rows for operator visibility.
        //
        // Also keep:
        // 1) one-cycle transient empty snapshots to prevent flash-empty glitches,
        // 2) rows outside truncated DB snapshot windows,
        // 3) event-only grants until TTL expiry (ID may arrive later).
        next.retain(|key, res| {
            if seen_active.contains(key) || res.released {
                return true;
            }
            if hold_active_rows {
                return true;
            }
            if snapshot_truncated && !res.released {
                return true;
            }
            if res.reservation_id.is_none() {
                let ttl_micros = i64::try_from(res.ttl_s)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000);
                let expires_micros = res.granted_ts.saturating_add(ttl_micros);
                return snapshot.timestamp_micros < expires_micros;
            }
            false
        });

        if self.reservations == next {
            return false;
        }

        self.reservations = next;
        true
    }

    fn refresh_from_db_fallback(&mut self, state: &TuiSharedState) -> bool {
        let database_url = state.config_snapshot().database_url;
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&database_url) {
            self.fallback_issue = Some(
                "DB snapshots are unavailable for :memory: SQLite URLs; use a file-backed DATABASE_URL for reservations visibility."
                    .to_string(),
            );
            return false;
        }

        let db_cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url,
            ..Default::default()
        };
        let path = match db_cfg.sqlite_path() {
            Ok(path) => path,
            Err(err) => {
                self.fallback_issue = Some(format!(
                    "Unable to parse SQLite path for reservations fallback: {err}"
                ));
                return false;
            }
        };

        let conn = match mcp_agent_mail_db::DbConn::open_file(&path) {
            Ok(conn) => conn,
            Err(err) => {
                self.fallback_issue = Some(format!(
                    "Unable to open DB for reservations fallback ({path}): {err}",
                ));
                return false;
            }
        };

        let rows = crate::tui_poller::fetch_reservation_snapshots(&conn);
        if rows.is_empty() {
            self.fallback_issue =
                Some("Direct DB fallback returned no active reservation rows.".to_string());
            return false;
        }

        self.fallback_issue = None;
        let fallback_snapshot = DbStatSnapshot {
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            file_reservations: u64::try_from(rows.len()).unwrap_or(u64::MAX),
            reservation_snapshots: rows,
            ..DbStatSnapshot::default()
        };
        self.apply_db_snapshot(&fallback_snapshot)
    }

    fn rebuild_sorted(&mut self) {
        let show_released = self.show_released;
        let now_micros = chrono::Utc::now().timestamp_micros();
        let mut entries: Vec<(&String, &ActiveReservation)> = self
            .reservations
            .iter()
            .filter(|(_, r)| show_released || !r.released)
            .collect();

        entries.sort_by(|(ka, a), (kb, b)| {
            let cmp = match self.sort_col {
                COL_AGENT => a.agent.to_lowercase().cmp(&b.agent.to_lowercase()),
                COL_PATH => a.path_pattern.cmp(&b.path_pattern),
                COL_EXCLUSIVE => a.exclusive.cmp(&b.exclusive),
                COL_TTL => a
                    .remaining_secs_at(now_micros)
                    .cmp(&b.remaining_secs_at(now_micros)),
                COL_PROJECT => a.project.to_lowercase().cmp(&b.project.to_lowercase()),
                _ => std::cmp::Ordering::Equal,
            };
            let cmp = cmp.then_with(|| ka.cmp(kb));
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
        self.prune_selection_to_visible();
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

    fn selected_reservation_keys_sorted(&self) -> Vec<String> {
        let mut keys = self.selected_reservation_keys.selected_items();
        keys.sort();
        keys
    }

    fn selected_reservation_ids_sorted(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .selected_reservation_keys_sorted()
            .iter()
            .filter_map(|key| {
                self.reservations
                    .get(key)
                    .and_then(|row| row.reservation_id)
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn prune_selection_to_visible(&mut self) {
        let visible_keys: HashSet<String> = self.sorted_keys.iter().cloned().collect();
        self.selected_reservation_keys
            .retain(|key| visible_keys.contains(key));
    }

    fn clear_reservation_selection(&mut self) {
        self.selected_reservation_keys.clear();
    }

    fn toggle_selection_for_cursor(&mut self) {
        if let Some(key) = self
            .table_state
            .selected
            .and_then(|idx| self.sorted_keys.get(idx))
            .cloned()
        {
            self.selected_reservation_keys.toggle(key);
        }
    }

    fn select_all_visible_reservations(&mut self) {
        self.selected_reservation_keys
            .select_all(self.sorted_keys.iter().cloned());
    }

    fn extend_visual_selection_to_cursor(&mut self) {
        if !self.selected_reservation_keys.visual_mode() {
            return;
        }
        if let Some(key) = self
            .table_state
            .selected
            .and_then(|idx| self.sorted_keys.get(idx))
            .cloned()
        {
            self.selected_reservation_keys.select(key);
        }
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

    fn row_index_from_mouse(&self, x: u16, y: u16) -> Option<usize> {
        let table = self.last_table_area.get();
        if table.width < 3 || table.height < 4 {
            return None;
        }
        if x <= table.x || x >= table.right().saturating_sub(1) {
            return None;
        }
        let first_data_row = table.y.saturating_add(2); // border + header row
        let last_data_row_exclusive = table.bottom().saturating_sub(1); // exclude bottom border
        if y < first_data_row || y >= last_data_row_exclusive {
            return None;
        }
        let visual_row = usize::from(y.saturating_sub(first_data_row));
        let absolute_row = self.last_render_offset.get().saturating_add(visual_row);
        (absolute_row < self.sorted_keys.len()).then_some(absolute_row)
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
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.move_selection(1);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.move_selection(-1);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        if !self.sorted_keys.is_empty() {
                            self.table_state.selected = Some(self.sorted_keys.len() - 1);
                            self.extend_visual_selection_to_cursor();
                        }
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        if !self.sorted_keys.is_empty() {
                            self.table_state.selected = Some(0);
                            self.extend_visual_selection_to_cursor();
                        }
                    }
                    KeyCode::Char(' ') => self.toggle_selection_for_cursor(),
                    KeyCode::Char('v') => {
                        let enabled = self.selected_reservation_keys.toggle_visual_mode();
                        if enabled {
                            self.extend_visual_selection_to_cursor();
                        }
                    }
                    KeyCode::Char('A') => self.select_all_visible_reservations(),
                    KeyCode::Char('C') => self.clear_reservation_selection(),
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
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                ftui::MouseEventKind::ScrollDown => {
                    self.move_selection(1);
                    self.extend_visual_selection_to_cursor();
                }
                ftui::MouseEventKind::ScrollUp => {
                    self.move_selection(-1);
                    self.extend_visual_selection_to_cursor();
                }
                ftui::MouseEventKind::Down(ftui::MouseButton::Left) => {
                    if let Some(row) = self.row_index_from_mouse(mouse.x, mouse.y) {
                        self.table_state.selected = Some(row);
                        self.extend_visual_selection_to_cursor();
                    }
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        let mut changed = false;
        let snapshot = state.db_stats_snapshot();
        if let Some(snapshot) = snapshot.clone() {
            changed |= self.apply_db_snapshot(&snapshot);
            if !snapshot.reservation_snapshots.is_empty() || snapshot.file_reservations == 0 {
                self.fallback_issue = None;
            }
        }
        changed |= self.ingest_events(state);
        let needs_fallback = snapshot.as_ref().is_some_and(|snap| {
            snap.reservation_snapshots.is_empty() && snap.file_reservations > 0
        });
        if needs_fallback
            && tick_count.saturating_sub(self.last_fallback_probe_tick) >= FALLBACK_DB_REFRESH_TICKS
        {
            self.last_fallback_probe_tick = tick_count;
            changed |= self.refresh_from_db_fallback(state);
        }
        if changed || tick_count % 10 == 0 {
            self.rebuild_sorted();
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let cursor_idx = self.table_state.selected?;
        let key = self.sorted_keys.get(cursor_idx)?;
        let reservation = self.reservations.get(key)?;
        let selected_keys = self.selected_reservation_keys_sorted();
        let reservation_ids = self.selected_reservation_ids_sorted();

        let actions = if selected_keys.len() > 1 {
            reservations_batch_actions(selected_keys.len(), &reservation_ids)
        } else {
            reservations_actions(
                reservation.reservation_id,
                &reservation.agent,
                &reservation.path_pattern,
            )
        };

        // Anchor row tracks the selected row within the visible viewport.
        let viewport_row = cursor_idx.saturating_sub(self.last_render_offset.get());
        let anchor_row = u16::try_from(viewport_row)
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let context_id = if selected_keys.len() > 1 {
            format!(
                "batch:{}",
                selected_keys
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            key.clone()
        };

        Some((actions, anchor_row, context_id))
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            self.last_table_area.set(Rect::new(0, 0, 0, 0));
            self.last_render_offset.set(0);
            return;
        }
        let tp = crate::tui_theme::TuiThemePalette::current();
        let effects_enabled = state.config_snapshot().tui_effects;
        let animation_time = state.uptime().as_secs_f64();

        let header_h = 1_u16;
        let table_h = area.height.saturating_sub(header_h);
        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);
        self.last_table_area.set(table_area);

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
        let selected_label = if self.selected_reservation_keys.is_empty() {
            String::new()
        } else {
            format!("  selected:{}", self.selected_reservation_keys.len())
        };
        let summary_base = format!(
            " {active} active  {exclusive} exclusive  {shared} shared{selected_label}   Sort: {sort_label}{sort_indicator} {released_label}",
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
            let start_offset =
                u16::try_from(display_width(summary_base.as_str())).unwrap_or(u16::MAX);
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
        let db_active_total = state
            .db_stats_snapshot()
            .and_then(|snapshot| usize::try_from(snapshot.file_reservations).ok())
            .unwrap_or(0);

        let mut ttl_overlay_rows: Vec<TtlOverlayRow> = Vec::new();
        let rows: Vec<Row> = self
            .sorted_keys
            .iter()
            .enumerate()
            .filter_map(|(i, key)| {
                let res = self.reservations.get(key)?;
                let batch_selected = self.selected_reservation_keys.contains(key);
                let checkbox = if batch_selected { "[x]" } else { "[ ]" };
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
                    selected: Some(i) == self.table_state.selected || batch_selected,
                    released: res.released,
                });

                let highlighted = Some(i) == self.table_state.selected || batch_selected;
                let style = if highlighted {
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
                        format!("{checkbox} {}", res.path_pattern),
                        excl_str.to_string(),
                        ttl_text,
                        res.project.clone(),
                    ])
                    .style(style),
                )
            })
            .collect();

        let block = Block::default()
            .title("Reservations")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(table_area);
        let width_cells = compute_table_widths(inner.width);
        let widths = [
            Constraint::Fixed(width_cells[COL_AGENT]),
            Constraint::Fixed(width_cells[COL_PATH]),
            Constraint::Fixed(width_cells[COL_EXCLUSIVE]),
            Constraint::Fixed(width_cells[COL_TTL]),
            Constraint::Fixed(width_cells[COL_PROJECT]),
        ];
        let rows_empty = rows.is_empty();
        let row_mismatch = rows_empty && !self.show_released && db_active_total > 0;

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
        self.last_render_offset.set(ts.offset);
        render_ttl_overlays(frame, table_area, &ttl_overlay_rows, ts.offset, &tp);
        if rows_empty && inner.height > 1 && inner.width > 4 {
            let text = if row_mismatch {
                let mut message = format!(
                    "DB reports {db_active_total} active reservations, but detail rows are unavailable. Poller snapshot is stale or failing."
                );
                if let Some(issue) = &self.fallback_issue {
                    message.push(' ');
                    message.push_str(issue);
                }
                message
            } else if let Some(issue) = &self.fallback_issue {
                issue.clone()
            } else if self.show_released {
                "No reservations match current filters.".to_string()
            } else {
                "No active reservations.".to_string()
            };
            let style = if row_mismatch || self.fallback_issue.is_some() {
                crate::tui_theme::text_warning(&tp)
            } else {
                crate::tui_theme::text_meta(&tp)
            };
            Paragraph::new(text).style(style).render(
                Rect::new(
                    inner.x,
                    inner.y.saturating_add(1),
                    inner.width,
                    inner.height.saturating_sub(1),
                ),
                frame,
            );
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate reservations",
            },
            HelpEntry {
                key: "Space",
                action: "Toggle selected reservation",
            },
            HelpEntry {
                key: "v / A / C",
                action: "Visual mode, select all, clear selection",
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
            HelpEntry {
                key: ".",
                action: "Open actions (single or batch)",
            },
            HelpEntry {
                key: "Mouse",
                action: "Wheel/Click navigate rows",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "File reservations held by agents. Space/v/A/C manage multi-select; use . for single/batch actions.",
        )
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

fn ttl_overlay_window_bounds(
    total_rows: usize,
    render_offset: usize,
    max_visible: usize,
) -> (usize, usize) {
    if total_rows == 0 || max_visible == 0 {
        return (0, 0);
    }
    let start = render_offset.min(total_rows);
    let end = start.saturating_add(max_visible).min(total_rows);
    (start, end)
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
    render_offset: usize,
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
    let (start, end) = ttl_overlay_window_bounds(rows.len(), render_offset, max_visible);
    for (idx, row) in rows[start..end].iter().enumerate() {
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
    use ftui_harness::buffer_to_text;
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
    fn empty_view_warns_when_db_reports_active_rows_but_none_loaded() {
        let state = test_state();
        state.update_db_stats(DbStatSnapshot {
            file_reservations: 5,
            timestamp_micros: 10,
            ..Default::default()
        });
        let screen = ReservationsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("DB reports 5 active reservations"),
            "missing mismatch warning text: {text}"
        );
    }

    #[test]
    fn tick_sets_fallback_issue_when_snapshot_rows_are_missing_for_memory_url() {
        let cfg = Config {
            database_url: "sqlite:///:memory:".to_string(),
            ..Config::default()
        };
        let state = TuiSharedState::new(&cfg);
        state.update_db_stats(DbStatSnapshot {
            file_reservations: 1,
            timestamp_micros: 1,
            ..Default::default()
        });

        let mut screen = ReservationsScreen::new();
        screen.tick(10, &state);

        let issue = screen
            .fallback_issue
            .as_deref()
            .expect("fallback issue should be set");
        assert!(
            issue.contains("file-backed DATABASE_URL"),
            "unexpected fallback issue text: {issue}"
        );
    }

    #[test]
    fn empty_view_includes_fallback_issue_context_when_rows_mismatch() {
        let cfg = Config {
            database_url: "sqlite:///:memory:".to_string(),
            ..Config::default()
        };
        let state = TuiSharedState::new(&cfg);
        state.update_db_stats(DbStatSnapshot {
            file_reservations: 1,
            timestamp_micros: 1,
            ..Default::default()
        });
        let mut screen = ReservationsScreen::new();
        screen.tick(10, &state);

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("DB reports 1 active reservations"),
            "missing mismatch warning text: {text}"
        );
        assert!(
            text.contains("DB snapshots are"),
            "missing fallback context text: {text}"
        );
    }

    #[test]
    fn empty_view_shows_no_active_when_db_count_is_zero() {
        let state = test_state();
        let screen = ReservationsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("No active reservations."),
            "missing empty-state text: {text}"
        );
    }

    #[test]
    fn empty_view_with_show_released_uses_filter_message() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        screen.show_released = true;
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("No reservations match current filters."),
            "missing filtered empty-state text: {text}"
        );
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
        assert!(bindings.len() >= 5);
        assert!(bindings.iter().any(|b| b.key == "Space"));
        assert!(bindings.iter().any(|b| b.key == "v / A / C"));
        assert!(bindings.iter().any(|b| b.key == "x"));
        assert!(bindings.iter().any(|b| b.key == "."));
        assert_eq!(
            screen.context_help_tip(),
            Some(
                "File reservations held by agents. Space/v/A/C manage multi-select; use . for single/batch actions.",
            )
        );
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
    fn space_toggles_reservation_selection() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        let key = reservation_key("proj", "BlueLake", "src/**");
        screen.reservations.insert(
            key.clone(),
            ActiveReservation {
                reservation_id: Some(1),
                agent: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 1_000_000,
                ttl_s: 3600,
                project: "proj".into(),
                released: false,
            },
        );
        screen.sorted_keys.push(key.clone());
        screen.table_state.selected = Some(0);

        let space = Event::Key(ftui::KeyEvent::new(KeyCode::Char(' ')));
        screen.update(&space, &state);
        assert!(screen.selected_reservation_keys.contains(&key));
        screen.update(&space, &state);
        assert!(!screen.selected_reservation_keys.contains(&key));
    }

    #[test]
    fn shift_a_and_shift_c_manage_reservation_selection() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        for (id, path) in [(1_i64, "src/**"), (2_i64, "tests/**")] {
            let key = reservation_key("proj", "BlueLake", path);
            screen.reservations.insert(
                key.clone(),
                ActiveReservation {
                    reservation_id: Some(id),
                    agent: "BlueLake".into(),
                    path_pattern: path.into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    ttl_s: 3600,
                    project: "proj".into(),
                    released: false,
                },
            );
            screen.sorted_keys.push(key);
        }
        screen.table_state.selected = Some(0);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('A'))), &state);
        assert_eq!(screen.selected_reservation_keys.len(), 2);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('C'))), &state);
        assert!(screen.selected_reservation_keys.is_empty());
        assert!(!screen.selected_reservation_keys.visual_mode());
    }

    #[test]
    fn visual_mode_extends_selection_on_navigation() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        for (id, path) in [(1_i64, "src/**"), (2_i64, "tests/**")] {
            let key = reservation_key("proj", "BlueLake", path);
            screen.reservations.insert(
                key.clone(),
                ActiveReservation {
                    reservation_id: Some(id),
                    agent: "BlueLake".into(),
                    path_pattern: path.into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    ttl_s: 3600,
                    project: "proj".into(),
                    released: false,
                },
            );
            screen.sorted_keys.push(key);
        }
        screen.table_state.selected = Some(0);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('v'))), &state);
        assert!(screen.selected_reservation_keys.visual_mode());
        assert_eq!(screen.selected_reservation_keys.len(), 1);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Down)), &state);
        assert_eq!(screen.selected_reservation_keys.len(), 2);
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

        let changed = screen.ingest_events(&state);
        assert!(changed);
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

        let changed = screen.ingest_events(&state);
        assert!(changed);
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
    fn ingest_release_all_active_marker_releases_all_agent_rows() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string(), "tests/**/*.rs".to_string()],
            true,
            3600,
            "proj",
        ));
        let _ = state.push_event(MailEvent::reservation_released(
            "BlueLake",
            vec!["<all-active>".to_string()],
            "proj",
        ));

        assert!(screen.ingest_events(&state));
        let (active, _, _, _) = screen.summary_counts();
        assert_eq!(active, 0);

        screen.show_released = true;
        screen.rebuild_sorted();
        assert_eq!(screen.sorted_keys.len(), 2);
    }

    #[test]
    fn ingest_release_id_token_matches_snapshot_reservation_id() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        state.update_db_stats(DbStatSnapshot {
            reservation_snapshots: vec![
                ReservationSnapshot {
                    id: 10,
                    project_slug: "proj".into(),
                    agent_name: "BlueLake".into(),
                    path_pattern: "src/**".into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    expires_ts: 4_000_000,
                    released_ts: None,
                },
                ReservationSnapshot {
                    id: 11,
                    project_slug: "proj".into(),
                    agent_name: "BlueLake".into(),
                    path_pattern: "tests/**".into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    expires_ts: 4_000_000,
                    released_ts: None,
                },
            ],
            timestamp_micros: 42,
            ..Default::default()
        });
        screen.tick(1, &state);

        let _ = state.push_event(MailEvent::reservation_released(
            "BlueLake",
            vec!["id:11".to_string()],
            "proj",
        ));
        assert!(screen.ingest_events(&state));

        let src_key = reservation_key("proj", "BlueLake", "src/**");
        let tests_key = reservation_key("proj", "BlueLake", "tests/**");
        assert!(
            !screen.reservations.get(&src_key).unwrap().released,
            "id:11 should not release src/**"
        );
        assert!(
            screen.reservations.get(&tests_key).unwrap().released,
            "id:11 should release tests/**"
        );
    }

    #[test]
    fn ingest_release_id_token_releases_single_event_only_candidate() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**".to_string()],
            true,
            3600,
            "proj",
        ));
        assert!(screen.ingest_events(&state));

        let _ = state.push_event(MailEvent::reservation_released(
            "BlueLake",
            vec!["id:77".to_string()],
            "proj",
        ));
        assert!(screen.ingest_events(&state));

        let key = reservation_key("proj", "BlueLake", "src/**");
        let row = screen.reservations.get(&key).expect("reservation row");
        assert!(row.released);
        assert_eq!(row.reservation_id, Some(77));
    }

    #[test]
    fn apply_db_snapshot_preserves_recent_event_only_grants() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        let _ = state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**/*.rs".to_string()],
            true,
            3600,
            "proj",
        ));
        assert!(screen.ingest_events(&state));
        assert_eq!(screen.reservations.len(), 1);

        // Snapshot with no rows and an older timestamp should not wipe the
        // event-derived grant.
        let changed = screen.apply_db_snapshot(&DbStatSnapshot {
            reservation_snapshots: vec![],
            timestamp_micros: 1,
            ..Default::default()
        });
        assert!(!changed);
        assert_eq!(screen.reservations.len(), 1);
    }

    #[test]
    fn apply_db_snapshot_reacquired_key_clears_stale_released_state() {
        let mut screen = ReservationsScreen::new();

        let key = reservation_key("proj", "BlueLake", "src/**");
        screen.reservations.insert(
            key.clone(),
            ActiveReservation {
                reservation_id: Some(9),
                agent: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 1_000_000,
                ttl_s: 10,
                project: "proj".into(),
                released: true,
            },
        );

        let changed = screen.apply_db_snapshot(&DbStatSnapshot {
            reservation_snapshots: vec![ReservationSnapshot {
                id: 10,
                project_slug: "proj".into(),
                agent_name: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 2_000_000,
                expires_ts: 8_000_000,
                released_ts: None,
            }],
            timestamp_micros: 42,
            ..Default::default()
        });

        assert!(changed);
        let row = screen
            .reservations
            .get(&key)
            .expect("reacquired snapshot row should exist");
        assert!(
            !row.released,
            "active snapshot row must not remain released"
        );
        let (active, _, _, _) = screen.summary_counts();
        assert_eq!(active, 1);
    }

    #[test]
    fn apply_db_snapshot_holds_rows_for_one_transient_empty_cycle() {
        let mut screen = ReservationsScreen::new();

        assert!(screen.apply_db_snapshot(&DbStatSnapshot {
            reservation_snapshots: vec![ReservationSnapshot {
                id: 10,
                project_slug: "proj".into(),
                agent_name: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 2_000_000,
                expires_ts: 8_000_000,
                released_ts: None,
            }],
            file_reservations: 1,
            timestamp_micros: 100,
            ..Default::default()
        }));
        assert_eq!(screen.reservations.len(), 1);

        // First empty snapshot is treated as transient to avoid flash-empty UI.
        let first_empty_changed = screen.apply_db_snapshot(&DbStatSnapshot {
            reservation_snapshots: vec![],
            file_reservations: 0,
            timestamp_micros: 101,
            ..Default::default()
        });
        assert!(!first_empty_changed);
        assert_eq!(screen.reservations.len(), 1);

        // Second consecutive empty snapshot confirms the clear.
        let second_empty_changed = screen.apply_db_snapshot(&DbStatSnapshot {
            reservation_snapshots: vec![],
            file_reservations: 0,
            timestamp_micros: 102,
            ..Default::default()
        });
        assert!(second_empty_changed);
        assert!(screen.reservations.is_empty());
    }

    #[test]
    fn contextual_actions_use_reservation_id_in_operation_payload() {
        let mut screen = ReservationsScreen::new();
        let key = reservation_key("proj", "BlueLake", "src/**");
        screen.reservations.insert(
            key.clone(),
            ActiveReservation {
                reservation_id: Some(77),
                agent: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 1_000_000,
                ttl_s: 3600,
                project: "proj".into(),
                released: false,
            },
        );
        screen.sorted_keys.push(key);
        screen.table_state.selected = Some(0);

        let (actions, _, _) = screen
            .contextual_actions()
            .expect("contextual actions should exist");

        let release = actions
            .iter()
            .find(|action| action.label == "Release")
            .expect("release action");
        match &release.action {
            crate::tui_action_menu::ActionKind::Execute(op) => {
                assert_eq!(op, "release:77");
            }
            other => panic!("expected Execute action, got {other:?}"),
        }
    }

    #[test]
    fn contextual_actions_switch_to_batch_for_multi_selected_rows() {
        let mut screen = ReservationsScreen::new();
        for (id, path) in [(22_i64, "src/**"), (11_i64, "tests/**")] {
            let key = reservation_key("proj", "BlueLake", path);
            screen.reservations.insert(
                key.clone(),
                ActiveReservation {
                    reservation_id: Some(id),
                    agent: "BlueLake".into(),
                    path_pattern: path.into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    ttl_s: 3600,
                    project: "proj".into(),
                    released: false,
                },
            );
            screen.sorted_keys.push(key.clone());
            screen.selected_reservation_keys.select(key);
        }
        screen.table_state.selected = Some(0);

        let (actions, _, context_id) = screen
            .contextual_actions()
            .expect("contextual actions should exist");
        assert!(context_id.starts_with("batch:"));
        assert!(
            actions
                .iter()
                .any(|a| a.label.starts_with("Release selected")),
            "expected batch release action",
        );
        let release = actions
            .iter()
            .find(|a| a.label.starts_with("Release selected"))
            .expect("release action");
        match &release.action {
            crate::tui_action_menu::ActionKind::ConfirmThenExecute { operation, .. } => {
                assert_eq!(operation, "release:11,22");
            }
            other => panic!("expected ConfirmThenExecute action, got {other:?}"),
        }
    }

    #[test]
    fn rebuild_sorted_ttl_ties_are_stable_by_key() {
        let mut screen = ReservationsScreen::new();
        screen.sort_col = COL_TTL;
        screen.sort_asc = true;

        // Equal TTL/granted timestamps force tie-breaking to key order.
        for path in ["z/**", "a/**", "m/**"] {
            let key = reservation_key("proj", "BlueLake", path);
            screen.reservations.insert(
                key,
                ActiveReservation {
                    reservation_id: None,
                    agent: "BlueLake".into(),
                    path_pattern: path.into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    ttl_s: 600,
                    project: "proj".into(),
                    released: false,
                },
            );
        }

        screen.rebuild_sorted();
        let mut expected = vec![
            reservation_key("proj", "BlueLake", "a/**"),
            reservation_key("proj", "BlueLake", "m/**"),
            reservation_key("proj", "BlueLake", "z/**"),
        ];
        expected.sort();
        assert_eq!(screen.sorted_keys, expected);
    }

    #[test]
    fn table_widths_cover_full_inner_width() {
        let widths = compute_table_widths(97);
        assert_eq!(widths.iter().copied().sum::<u16>(), 97);
        assert_eq!(widths[COL_TTL], 29);
    }

    #[test]
    fn ttl_overlay_window_bounds_respects_offset_and_capacity() {
        assert_eq!(ttl_overlay_window_bounds(0, 0, 4), (0, 0));
        assert_eq!(ttl_overlay_window_bounds(10, 0, 3), (0, 3));
        assert_eq!(ttl_overlay_window_bounds(10, 4, 3), (4, 7));
        assert_eq!(ttl_overlay_window_bounds(10, 9, 3), (9, 10));
        assert_eq!(ttl_overlay_window_bounds(10, 42, 3), (10, 10));
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
        let changed = screen.ingest_events(&state);
        assert!(changed);
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

        let changed = screen.ingest_events(&state);
        assert!(changed);
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

    #[test]
    fn applies_db_snapshot_on_first_tick() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();

        state.update_db_stats(DbStatSnapshot {
            reservation_snapshots: vec![
                ReservationSnapshot {
                    id: 10,
                    project_slug: "proj".into(),
                    agent_name: "BlueLake".into(),
                    path_pattern: "src/**".into(),
                    exclusive: true,
                    granted_ts: 1_000_000,
                    expires_ts: 4_000_000,
                    released_ts: None,
                },
                ReservationSnapshot {
                    id: 11,
                    project_slug: "proj".into(),
                    agent_name: "RedStone".into(),
                    path_pattern: "tests/**".into(),
                    exclusive: false,
                    granted_ts: 1_000_000,
                    expires_ts: 7_000_000,
                    released_ts: None,
                },
            ],
            timestamp_micros: 42,
            ..Default::default()
        });

        screen.tick(1, &state);

        assert_eq!(screen.reservations.len(), 2);
        assert_eq!(screen.last_snapshot_micros, 42);
        assert!(!screen.sorted_keys.is_empty());
    }
}
