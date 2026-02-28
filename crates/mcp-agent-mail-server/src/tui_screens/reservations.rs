//! Reservations screen — active file reservations with TTL progress bars.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use asupersync::Cx;
use fastmcp::prelude::McpContext;
use fastmcp_core::block_on;
use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::text::display_width;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_extras::text_effects::{StyledText, TextEffect};
use ftui_runtime::program::Cmd;
use ftui_widgets::input::TextInput;
use ftui_widgets::progress::ProgressBar;
use ftui_widgets::textarea::TextArea;
use serde::Deserialize;

use crate::tui_action_menu::{ActionEntry, reservations_actions, reservations_batch_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{DbStatSnapshot, MailEvent, ReservationSnapshot};
use crate::tui_persist::{
    ScreenFilterPresetStore, console_persist_path_from_env_or_default,
    load_screen_filter_presets_or_default, save_screen_filter_presets, screen_filter_presets_path,
};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg, SelectionState};
use crate::tui_widgets::fancy::SummaryFooter;
use crate::tui_widgets::{MetricTile, MetricTrend};

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
const RESERVATION_TTL_PRESETS: [(&str, i64); 5] = [
    ("1h", 3600),
    ("4h", 14_400),
    ("12h", 43_200),
    ("24h", 86_400),
    ("Custom", 0),
];
const RESERVATION_TTL_CUSTOM_INDEX: usize = RESERVATION_TTL_PRESETS.len() - 1;
const RESERVATION_PATH_MIN_ROWS: u16 = 4;
const RESERVATIONS_PRESET_SCREEN_ID: &str = "reservations";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationCreateField {
    Paths,
    Exclusive,
    Ttl,
    CustomTtl,
    Reason,
}

impl ReservationCreateField {
    const fn next(self, custom_ttl_enabled: bool) -> Self {
        match self {
            Self::Paths => Self::Exclusive,
            Self::Exclusive => Self::Ttl,
            Self::Ttl => {
                if custom_ttl_enabled {
                    Self::CustomTtl
                } else {
                    Self::Reason
                }
            }
            Self::CustomTtl => Self::Reason,
            Self::Reason => Self::Paths,
        }
    }

    const fn prev(self, custom_ttl_enabled: bool) -> Self {
        match self {
            Self::Paths => Self::Reason,
            Self::Exclusive => Self::Paths,
            Self::Ttl => Self::Exclusive,
            Self::CustomTtl => Self::Ttl,
            Self::Reason => {
                if custom_ttl_enabled {
                    Self::CustomTtl
                } else {
                    Self::Ttl
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetDialogMode {
    None,
    Save,
    Load,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePresetField {
    Name,
    Description,
}

impl SavePresetField {
    const fn next(self) -> Self {
        match self {
            Self::Name => Self::Description,
            Self::Description => Self::Name,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ReservationCreateValidationErrors {
    paths: Option<String>,
    ttl: Option<String>,
    general: Option<String>,
}

impl ReservationCreateValidationErrors {
    const fn has_any(&self) -> bool {
        self.paths.is_some() || self.ttl.is_some() || self.general.is_some()
    }
}

struct ReservationCreateFormState {
    project_slug: String,
    agent_name: String,
    paths_input: TextArea,
    custom_ttl_input: TextInput,
    reason_input: TextInput,
    exclusive: bool,
    ttl_idx: usize,
    focus: ReservationCreateField,
    errors: ReservationCreateValidationErrors,
}

impl ReservationCreateFormState {
    fn new(project_slug: String, agent_name: String) -> Self {
        let mut form = Self {
            project_slug,
            agent_name,
            paths_input: TextArea::new()
                .with_placeholder("One path/glob per line (e.g., crates/**, src/*.rs)"),
            custom_ttl_input: TextInput::new().with_placeholder("e.g. 90m or 2h"),
            reason_input: TextInput::new().with_placeholder("Optional reason (e.g., br-3oavg)"),
            exclusive: true,
            ttl_idx: 0,
            focus: ReservationCreateField::Paths,
            errors: ReservationCreateValidationErrors::default(),
        };
        form.update_focus();
        form
    }

    const fn custom_ttl_enabled(&self) -> bool {
        self.ttl_idx == RESERVATION_TTL_CUSTOM_INDEX
    }

    fn update_focus(&mut self) {
        self.paths_input
            .set_focused(matches!(self.focus, ReservationCreateField::Paths));
        self.custom_ttl_input
            .set_focused(matches!(self.focus, ReservationCreateField::CustomTtl));
        self.reason_input
            .set_focused(matches!(self.focus, ReservationCreateField::Reason));
    }

    fn cycle_focus_next(&mut self) {
        self.focus = self.focus.next(self.custom_ttl_enabled());
        self.update_focus();
    }

    fn cycle_focus_prev(&mut self) {
        self.focus = self.focus.prev(self.custom_ttl_enabled());
        self.update_focus();
    }

    fn paths(&self) -> Vec<String> {
        self.paths_input
            .text()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect()
    }
}

struct ReservationCreatePayload {
    project_key: String,
    agent_name: String,
    paths: Vec<String>,
    ttl_seconds: i64,
    exclusive: bool,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReservationCreateToolResponse {
    granted: Vec<ReservationCreateGranted>,
    conflicts: Vec<ReservationCreateConflict>,
}

#[derive(Debug, Deserialize)]
struct ReservationCreateGranted {
    path_pattern: String,
}

#[derive(Debug, Deserialize)]
struct ReservationCreateConflict {
    path: String,
}

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
    /// Previous summary counts for `MetricTrend` computation.
    prev_counts: (u64, u64, u64, u64),
    /// Reservation create modal form state.
    create_form: Option<ReservationCreateFormState>,
    /// On-disk path for persisted screen filter presets.
    filter_presets_path: PathBuf,
    /// Preset store loaded from `filter_presets_path`.
    filter_presets: ScreenFilterPresetStore,
    /// Active preset dialog mode (save/load/none).
    preset_dialog_mode: PresetDialogMode,
    /// Save dialog field focus.
    save_preset_field: SavePresetField,
    /// Save dialog: preset name input buffer.
    save_preset_name: String,
    /// Save dialog: optional description input buffer.
    save_preset_description: String,
    /// Load dialog selected preset row.
    load_preset_cursor: usize,
    /// Whether the detail panel is visible on wide screens.
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
    /// Last observed data-channel generation for dirty-state gating.
    last_data_gen: super::DataGeneration,
}

impl ReservationsScreen {
    fn build(filter_presets_path_override: Option<PathBuf>) -> Self {
        let filter_presets_path = filter_presets_path_override.unwrap_or_else(|| {
            let console_path = console_persist_path_from_env_or_default();
            screen_filter_presets_path(&console_path)
        });
        let filter_presets = load_screen_filter_presets_or_default(&filter_presets_path);
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
            prev_counts: (0, 0, 0, 0),
            create_form: None,
            filter_presets_path,
            filter_presets,
            preset_dialog_mode: PresetDialogMode::None,
            save_preset_field: SavePresetField::Name,
            save_preset_name: String::new(),
            save_preset_description: String::new(),
            load_preset_cursor: 0,
            detail_visible: true,
            detail_scroll: 0,
            last_data_gen: super::DataGeneration::stale(),
        }
    }

    #[cfg(test)]
    fn with_filter_presets_path_for_test(path: &std::path::Path) -> Self {
        Self::build(Some(path.to_path_buf()))
    }

    #[must_use]
    pub fn new() -> Self {
        Self::build(None)
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

        if let Some(id_str) = token.strip_prefix("id:")
            && let Ok(target_id) = id_str.parse::<i64>()
        {
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
                .filter(|(_, res)| res.project == project && res.agent == agent && !res.released)
                .collect();
            if candidates.len() == 1 {
                let (_, res) = candidates.remove(0);
                res.released = true;
                res.reservation_id = Some(target_id);
                return true;
            }
            return changed;
        }

        let key = reservation_key(project, agent, token);
        if let Some(res) = self.reservations.get_mut(&key)
            && !res.released
        {
            res.released = true;
            return true;
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

        let conn = match mcp_agent_mail_db::open_sqlite_file_with_recovery(&path) {
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
        if let Some(sel) = self.table_state.selected
            && sel >= self.sorted_keys.len()
        {
            self.table_state.selected = if self.sorted_keys.is_empty() {
                None
            } else {
                Some(self.sorted_keys.len() - 1)
            };
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
        self.detail_scroll = 0;
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

    #[allow(clippy::cast_possible_truncation)]
    /// Render the detail panel for the selected reservation.
    fn render_reservation_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Reservation Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(selected_idx) = self.table_state.selected else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f512}",
                "No Reservation Selected",
                "Select a reservation from the table to view details.",
            );
            return;
        };

        let Some(key) = self.sorted_keys.get(selected_idx) else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f512}",
                "No Reservation Selected",
                "Select a reservation from the table to view details.",
            );
            return;
        };

        let Some(res) = self.reservations.get(key) else {
            return;
        };

        let remaining = res.remaining_secs();
        let ratio = res.ttl_ratio();

        let mut lines: Vec<(String, String, Option<PackedRgba>)> = vec![
            ("Agent".into(), res.agent.clone(), None),
            ("Pattern".into(), res.path_pattern.clone(), None),
            ("Project".into(), res.project.clone(), None),
            (
                "Exclusive".into(),
                if res.exclusive {
                    "\u{2713} Yes"
                } else {
                    "\u{2717} No"
                }
                .into(),
                Some(if res.exclusive {
                    tp.severity_warn
                } else {
                    tp.text_primary
                }),
            ),
        ];

        let ttl_str = format_ttl(remaining);
        let ttl_color = if remaining == 0 {
            tp.severity_error
        } else if ratio < 0.2 {
            tp.ttl_warning
        } else {
            tp.text_primary
        };
        lines.push(("TTL Left".into(), ttl_str, Some(ttl_color)));
        lines.push(("TTL Total".into(), format!("{}s", res.ttl_s), None));

        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
        let pct = (ratio * 100.0) as u64;
        lines.push(("TTL Ratio".into(), format!("{pct}%"), None));

        let granted_str = mcp_agent_mail_db::timestamps::micros_to_iso(res.granted_ts);
        lines.push(("Granted At".into(), granted_str, None));

        if res.released {
            lines.push(("Status".into(), "Released".into(), Some(tp.text_muted)));
        } else if remaining == 0 {
            lines.push(("Status".into(), "Expired".into(), Some(tp.severity_error)));
        } else {
            lines.push(("Status".into(), "Active".into(), Some(tp.activity_active)));
        }

        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }

    fn render_summary_band(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let (active, exclusive, shared, expired) = self.summary_counts();
        let (prev_active, prev_excl, prev_shared, prev_expired) = self.prev_counts;

        let active_str = active.to_string();
        let excl_str = exclusive.to_string();
        let shared_str = shared.to_string();
        let expired_str = expired.to_string();

        let tiles: Vec<(&str, &str, MetricTrend, PackedRgba)> = vec![
            (
                "Active",
                &active_str,
                trend_for(active as u64, prev_active),
                tp.ttl_healthy,
            ),
            (
                "Exclusive",
                &excl_str,
                trend_for(exclusive as u64, prev_excl),
                tp.metric_reservations,
            ),
            (
                "Shared",
                &shared_str,
                trend_for(shared as u64, prev_shared),
                tp.metric_agents,
            ),
            (
                "Expired",
                &expired_str,
                trend_for(expired as u64, prev_expired),
                tp.ttl_danger,
            ),
        ];

        let tile_count = tiles.len();
        if tile_count == 0 || area.width == 0 || area.height == 0 {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let tile_w = area.width / (tile_count as u16);

        for (i, (label, value, trend, color)) in tiles.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
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

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let (active, exclusive, shared, expired) = self.summary_counts();

        let active_str = active.to_string();
        let excl_str = exclusive.to_string();
        let shared_str = shared.to_string();
        let expired_str = expired.to_string();

        let items: Vec<(&str, &str, PackedRgba)> = vec![
            (&*active_str, "active", tp.ttl_healthy),
            (&*excl_str, "exclusive", tp.metric_reservations),
            (&*shared_str, "shared", tp.metric_agents),
            (&*expired_str, "expired", tp.ttl_danger),
        ];

        SummaryFooter::new(&items, tp.text_muted).render(area, frame);
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

    fn infer_create_form_context(&self) -> (String, String) {
        let selected = self
            .table_state
            .selected
            .and_then(|idx| self.sorted_keys.get(idx))
            .and_then(|key| self.reservations.get(key))
            .map(|res| (res.project.clone(), res.agent.clone()));
        if let Some(ctx) = selected {
            return ctx;
        }

        self.sorted_keys
            .iter()
            .find_map(|key| self.reservations.get(key))
            .map_or_else(
                || (String::new(), String::new()),
                |res| (res.project.clone(), res.agent.clone()),
            )
    }

    fn open_create_form(&mut self) {
        let (project_slug, agent_name) = self.infer_create_form_context();
        let mut form = ReservationCreateFormState::new(project_slug, agent_name);
        if form.project_slug.is_empty() || form.agent_name.is_empty() {
            form.errors.general = Some(
                "Select a reservation row first so project/agent context can be inferred."
                    .to_string(),
            );
        }
        self.create_form = Some(form);
    }

    fn preset_names(&self) -> Vec<String> {
        self.filter_presets
            .list_names(RESERVATIONS_PRESET_SCREEN_ID)
    }

    fn persist_filter_presets(&self) {
        if let Err(err) =
            save_screen_filter_presets(&self.filter_presets_path, &self.filter_presets)
        {
            eprintln!(
                "reservations: failed to save presets to {}: {err}",
                self.filter_presets_path.display()
            );
        }
    }

    fn snapshot_filter_values(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert("sort_col".to_string(), self.sort_col.to_string());
        values.insert("sort_asc".to_string(), self.sort_asc.to_string());
        values.insert("show_released".to_string(), self.show_released.to_string());
        values
    }

    fn save_named_preset(&mut self, name: &str, description: Option<String>) -> bool {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return false;
        }
        let trimmed_description = description.and_then(|text| {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        self.filter_presets.upsert(
            RESERVATIONS_PRESET_SCREEN_ID,
            trimmed_name.to_string(),
            trimmed_description,
            self.snapshot_filter_values(),
        );
        self.persist_filter_presets();
        true
    }

    fn apply_preset_values(&mut self, values: &BTreeMap<String, String>) {
        if let Some(sort_col) = values
            .get("sort_col")
            .and_then(|raw| raw.parse::<usize>().ok())
        {
            self.sort_col = sort_col.min(SORT_LABELS.len().saturating_sub(1));
        }
        if let Some(sort_asc) = values.get("sort_asc") {
            self.sort_asc = matches!(
                sort_asc.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        if let Some(show_released) = values.get("show_released") {
            self.show_released = matches!(
                show_released.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        self.rebuild_sorted();
    }

    fn apply_named_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self
            .filter_presets
            .get(RESERVATIONS_PRESET_SCREEN_ID, name)
            .cloned()
        else {
            return false;
        };
        self.apply_preset_values(&preset.values);
        true
    }

    fn remove_named_preset(&mut self, name: &str) -> bool {
        let removed = self
            .filter_presets
            .remove(RESERVATIONS_PRESET_SCREEN_ID, name);
        if removed {
            self.persist_filter_presets();
        }
        removed
    }

    fn open_save_preset_dialog(&mut self) {
        self.preset_dialog_mode = PresetDialogMode::Save;
        self.save_preset_field = SavePresetField::Name;
        self.save_preset_description.clear();
        if self.save_preset_name.is_empty() {
            self.save_preset_name = "reservations-preset".to_string();
        }
    }

    fn open_load_preset_dialog(&mut self) {
        self.preset_dialog_mode = PresetDialogMode::Load;
        let names = self.preset_names();
        if names.is_empty() {
            self.load_preset_cursor = 0;
        } else {
            self.load_preset_cursor = self.load_preset_cursor.min(names.len().saturating_sub(1));
        }
    }

    fn handle_save_dialog_key(&mut self, key: &ftui::KeyEvent) {
        match key.code {
            KeyCode::Escape => {
                self.preset_dialog_mode = PresetDialogMode::None;
            }
            KeyCode::Tab => {
                self.save_preset_field = self.save_preset_field.next();
            }
            KeyCode::Backspace => match self.save_preset_field {
                SavePresetField::Name => {
                    self.save_preset_name.pop();
                }
                SavePresetField::Description => {
                    self.save_preset_description.pop();
                }
            },
            KeyCode::Enter => {
                let preset_name = self.save_preset_name.clone();
                if self.save_named_preset(&preset_name, Some(self.save_preset_description.clone()))
                {
                    self.preset_dialog_mode = PresetDialogMode::None;
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(Modifiers::CTRL) => {
                match self.save_preset_field {
                    SavePresetField::Name => self.save_preset_name.push(ch),
                    SavePresetField::Description => self.save_preset_description.push(ch),
                }
            }
            _ => {}
        }
    }

    fn handle_load_dialog_key(&mut self, key: &ftui::KeyEvent) {
        let names = self.preset_names();
        match key.code {
            KeyCode::Escape => {
                self.preset_dialog_mode = PresetDialogMode::None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !names.is_empty() {
                    self.load_preset_cursor = (self.load_preset_cursor + 1).min(names.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.load_preset_cursor = self.load_preset_cursor.saturating_sub(1);
            }
            KeyCode::Delete => {
                if let Some(name) = names.get(self.load_preset_cursor) {
                    let _ = self.remove_named_preset(name);
                }
                let refreshed = self.preset_names();
                if refreshed.is_empty() {
                    self.load_preset_cursor = 0;
                } else {
                    self.load_preset_cursor = self
                        .load_preset_cursor
                        .min(refreshed.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(name) = names.get(self.load_preset_cursor) {
                    let _ = self.apply_named_preset(name);
                    self.preset_dialog_mode = PresetDialogMode::None;
                }
            }
            _ => {}
        }
    }

    fn parse_custom_ttl_seconds(raw: &str) -> Result<i64, String> {
        let value = raw.trim();
        if value.is_empty() {
            return Err("Custom TTL is required (example: 90m or 2h).".to_string());
        }
        if value.len() < 2 {
            return Err("Custom TTL must include a numeric value and suffix (m or h).".to_string());
        }

        let (num_part, unit_part) = value.split_at(value.len() - 1);
        let qty = num_part
            .trim()
            .parse::<i64>()
            .map_err(|_| "Custom TTL must start with a positive integer.".to_string())?;
        if qty <= 0 {
            return Err("Custom TTL must be greater than zero.".to_string());
        }

        let unit = unit_part.to_ascii_lowercase();
        let multiplier = match unit.as_str() {
            "m" => 60_i64,
            "h" => 3600_i64,
            _ => {
                return Err("Custom TTL suffix must be 'm' (minutes) or 'h' (hours).".to_string());
            }
        };
        let seconds = qty
            .checked_mul(multiplier)
            .ok_or_else(|| "Custom TTL is too large.".to_string())?;
        if seconds < 60 {
            return Err("TTL must be at least 60 seconds.".to_string());
        }
        Ok(seconds)
    }

    fn validate_create_form(
        form: &ReservationCreateFormState,
    ) -> Result<ReservationCreatePayload, ReservationCreateValidationErrors> {
        let mut errors = ReservationCreateValidationErrors::default();

        if form.project_slug.trim().is_empty() || form.agent_name.trim().is_empty() {
            errors.general = Some(
                "Unable to infer project/agent context. Select a reservation row first."
                    .to_string(),
            );
        }

        let paths = form.paths();
        if paths.is_empty() {
            errors.paths = Some("Provide at least one path/glob pattern.".to_string());
        }

        let ttl_seconds = if form.custom_ttl_enabled() {
            match Self::parse_custom_ttl_seconds(form.custom_ttl_input.value()) {
                Ok(v) => v,
                Err(err) => {
                    errors.ttl = Some(err);
                    0
                }
            }
        } else {
            RESERVATION_TTL_PRESETS
                .get(form.ttl_idx)
                .map_or(3600, |(_, secs)| *secs)
        };

        if errors.has_any() {
            return Err(errors);
        }

        let reason = form.reason_input.value().trim();
        Ok(ReservationCreatePayload {
            project_key: form.project_slug.trim().to_string(),
            agent_name: form.agent_name.trim().to_string(),
            paths,
            ttl_seconds,
            exclusive: form.exclusive,
            reason: (!reason.is_empty()).then(|| reason.to_string()),
        })
    }

    fn submit_create_form(&mut self) -> Cmd<MailScreenMsg> {
        let (payload, errors) = match self.create_form.as_ref() {
            Some(form) => match Self::validate_create_form(form) {
                Ok(payload) => (Some(payload), ReservationCreateValidationErrors::default()),
                Err(errors) => (None, errors),
            },
            None => return Cmd::None,
        };

        if errors.has_any() {
            if let Some(form) = self.create_form.as_mut() {
                form.errors = errors;
            }
            return Cmd::None;
        }

        let Some(payload) = payload else {
            return Cmd::None;
        };

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = block_on(mcp_agent_mail_tools::reservations::file_reservation_paths(
            &ctx,
            payload.project_key.clone(),
            payload.agent_name.clone(),
            payload.paths.clone(),
            Some(payload.ttl_seconds),
            Some(payload.exclusive),
            payload.reason,
        ));

        match result {
            Ok(raw_json) => {
                self.create_form = None;
                let parsed = serde_json::from_str::<ReservationCreateToolResponse>(&raw_json).ok();
                let (status, summary) = if let Some(resp) = parsed {
                    let granted_count = resp.granted.len();
                    let conflict_count = resp.conflicts.len();
                    let granted_hint = resp.granted.first().map_or(String::new(), |row| {
                        format!(" (e.g., {})", row.path_pattern)
                    });
                    let conflict_hint = resp
                        .conflicts
                        .first()
                        .map_or(String::new(), |row| format!(" (first: {})", row.path));
                    if conflict_count > 0 {
                        (
                            "warn",
                            format!(
                                "{granted_count} granted{granted_hint}, {conflict_count} conflicts{conflict_hint}"
                            ),
                        )
                    } else {
                        ("ok", format!("{granted_count} granted{granted_hint}"))
                    }
                } else {
                    ("ok", "Reservation request completed.".to_string())
                };
                Cmd::msg(MailScreenMsg::ActionExecute(
                    format!("reservation_create_result:{status}"),
                    summary,
                ))
            }
            Err(err) => {
                let message = err.to_string();
                if let Some(form) = self.create_form.as_mut() {
                    form.errors.general = Some(message.clone());
                }
                Cmd::msg(MailScreenMsg::ActionExecute(
                    "reservation_create_result:error".to_string(),
                    message,
                ))
            }
        }
    }

    fn handle_create_form_event(&mut self, event: &Event) -> Cmd<MailScreenMsg> {
        let Event::Key(key) = event else {
            return Cmd::None;
        };
        if key.kind != KeyEventKind::Press {
            return Cmd::None;
        }

        let ctrl_enter = key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Enter;
        if ctrl_enter || key.code == KeyCode::F(5) {
            return self.submit_create_form();
        }
        if key.code == KeyCode::Escape {
            self.create_form = None;
            return Cmd::None;
        }
        if key.code == KeyCode::Tab {
            if let Some(form) = self.create_form.as_mut() {
                form.cycle_focus_next();
            }
            return Cmd::None;
        }
        if key.code == KeyCode::BackTab {
            if let Some(form) = self.create_form.as_mut() {
                form.cycle_focus_prev();
            }
            return Cmd::None;
        }

        let Some(form) = self.create_form.as_mut() else {
            return Cmd::None;
        };

        match form.focus {
            ReservationCreateField::Paths => {
                let before = form.paths_input.text();
                let _ = form.paths_input.handle_event(event);
                if form.paths_input.text() != before {
                    form.errors.paths = None;
                    form.errors.general = None;
                }
            }
            ReservationCreateField::Exclusive => match key.code {
                KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
                    form.exclusive = !form.exclusive;
                    form.errors.general = None;
                }
                _ => {}
            },
            ReservationCreateField::Ttl => match key.code {
                KeyCode::Left | KeyCode::Up => {
                    if form.ttl_idx == 0 {
                        form.ttl_idx = RESERVATION_TTL_PRESETS.len() - 1;
                    } else {
                        form.ttl_idx -= 1;
                    }
                    form.errors.ttl = None;
                    form.errors.general = None;
                    form.update_focus();
                }
                KeyCode::Right | KeyCode::Down => {
                    form.ttl_idx = (form.ttl_idx + 1) % RESERVATION_TTL_PRESETS.len();
                    form.errors.ttl = None;
                    form.errors.general = None;
                    form.update_focus();
                }
                KeyCode::Enter => form.cycle_focus_next(),
                _ => {}
            },
            ReservationCreateField::CustomTtl => {
                let before = form.custom_ttl_input.value().to_string();
                let _ = form.custom_ttl_input.handle_event(event);
                if form.custom_ttl_input.value() != before {
                    form.errors.ttl = None;
                    form.errors.general = None;
                }
            }
            ReservationCreateField::Reason => {
                let before = form.reason_input.value().to_string();
                let _ = form.reason_input.handle_event(event);
                if form.reason_input.value() != before {
                    form.errors.general = None;
                }
            }
        }
        Cmd::None
    }
}

impl Default for ReservationsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ReservationsScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if self.create_form.is_some() {
            return self.handle_create_form_event(event);
        }

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            if self.preset_dialog_mode != PresetDialogMode::None {
                match self.preset_dialog_mode {
                    PresetDialogMode::Save => self.handle_save_dialog_key(key),
                    PresetDialogMode::Load => self.handle_load_dialog_key(key),
                    PresetDialogMode::None => {}
                }
                return Cmd::None;
            }

            if key.modifiers.contains(Modifiers::CTRL) {
                match key.code {
                    KeyCode::Char('s') => {
                        self.open_save_preset_dialog();
                        return Cmd::None;
                    }
                    KeyCode::Char('l') => {
                        self.open_load_preset_dialog();
                        return Cmd::None;
                    }
                    _ => {}
                }
            }

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
                KeyCode::Char('n') => self.open_create_form(),
                KeyCode::Char('i') => {
                    self.detail_visible = !self.detail_visible;
                }
                KeyCode::Char('J') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                _ => {}
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
        // ── Dirty-state gated data ingestion ────────────────────────
        let current_gen = state.data_generation();
        let dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        let mut changed = false;
        if dirty.db_stats {
            let snapshot = state.db_stats_snapshot();
            if let Some(snapshot) = snapshot.clone() {
                changed |= self.apply_db_snapshot(&snapshot);
                if !snapshot.reservation_snapshots.is_empty() || snapshot.file_reservations == 0 {
                    self.fallback_issue = None;
                }
            }
            let needs_fallback = snapshot.as_ref().is_some_and(|snap| {
                snap.reservation_snapshots.is_empty() && snap.file_reservations > 0
            });
            if needs_fallback
                && tick_count.saturating_sub(self.last_fallback_probe_tick)
                    >= FALLBACK_DB_REFRESH_TICKS
            {
                self.last_fallback_probe_tick = tick_count;
                changed |= self.refresh_from_db_fallback(state);
            }
        }
        if dirty.events {
            changed |= self.ingest_events(state);
        }
        if changed || (tick_count.is_multiple_of(10) && dirty.any()) {
            let (a, e, s, x) = self.summary_counts();
            self.prev_counts = (a as u64, e as u64, s as u64, x as u64);
            self.rebuild_sorted();
        }
        self.sync_focused_event();

        self.last_data_gen = current_gen;
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

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            self.last_table_area.set(Rect::new(0, 0, 0, 0));
            self.last_render_offset.set(0);
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();

        // Outer bordered panel wrapping entire screen
        let outer_block = crate::tui_panel_helpers::panel_block(" File Reservations ");
        let inner = outer_block.inner(area);
        outer_block.render(area, frame);
        let area = inner;

        // Detail panel hidden when modal forms are active
        let modal_active =
            self.create_form.is_some() || self.preset_dialog_mode != PresetDialogMode::None;

        // Responsive layout: table+detail on wide screens (unless modal is active)
        let show_side_detail = self.detail_visible && !modal_active;
        let layout = if show_side_detail {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
                .at(
                    Breakpoint::Lg,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(55.0), Constraint::Fill]),
                )
                .at(
                    Breakpoint::Xl,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(50.0), Constraint::Fill]),
                )
        } else {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
        };

        let split = layout.split(area);
        let area = split.rects[0];

        let effects_enabled = state.config_snapshot().tui_effects;
        let animation_time = state.uptime().as_secs_f64();
        let wide = area.width >= 120;
        let narrow = area.width < 80;

        // Layout: summary_band(2) + header(1) + table(remainder) + footer(1)
        let summary_h: u16 = if area.height >= 10 { 2 } else { 0 };
        let header_h: u16 = 1;
        let footer_h = u16::from(area.height >= 6);
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
                    Paragraph::new(critical_alert.trim_start().to_string())
                        .style(crate::tui_theme::text_critical(&tp))
                        .render(alert_area, frame);
                }
            }
        }

        // ── Table ──────────────────────────────────────────────────────
        let table_area = Rect::new(area.x, y, area.width, table_h);
        y += table_h;
        self.last_table_area.set(table_area);

        // Responsive column headers and widths
        let (header_cells, col_widths): (Vec<&str>, Vec<Constraint>) = if narrow {
            // < 80: hide Project column, compact
            (
                vec!["Agent", "Path Pattern", "Excl", "TTL Remaining"],
                vec![
                    Constraint::Percentage(22.0),
                    Constraint::Percentage(38.0),
                    Constraint::Percentage(10.0),
                    Constraint::Percentage(30.0),
                ],
            )
        } else if wide {
            // >= 120: all 5 columns
            (
                vec!["Agent", "Path Pattern", "Excl", "TTL Remaining", "Project"],
                vec![
                    Constraint::Percentage(18.0),
                    Constraint::Percentage(27.0),
                    Constraint::Percentage(8.0),
                    Constraint::Percentage(30.0),
                    Constraint::Percentage(17.0),
                ],
            )
        } else {
            // 80–119: all 5, reduced Path
            (
                vec!["Agent", "Path Pattern", "Excl", "TTL Remaining", "Project"],
                vec![
                    Constraint::Percentage(18.0),
                    Constraint::Percentage(22.0),
                    Constraint::Percentage(8.0),
                    Constraint::Percentage(32.0),
                    Constraint::Percentage(20.0),
                ],
            )
        };

        let header = Row::new(header_cells).style(Style::default().bold());
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

                if narrow {
                    Some(
                        Row::new([
                            res.agent.clone(),
                            format!("{checkbox} {}", res.path_pattern),
                            excl_str.to_string(),
                            ttl_text,
                        ])
                        .style(style),
                    )
                } else {
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
                }
            })
            .collect();

        let block = Block::default()
            .title("Reservations")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(table_area);
        let rows_empty = rows.is_empty();
        let row_mismatch = rows_empty && !self.show_released && db_active_total > 0;

        let table = Table::new(rows, col_widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
        self.last_render_offset.set(ts.offset);
        if !narrow {
            render_ttl_overlays(frame, table_area, &ttl_overlay_rows, ts.offset, &tp);
        }
        if rows_empty && inner.height > 1 && inner.width > 4 {
            if row_mismatch {
                let mut message = format!(
                    "DB reports {db_active_total} active reservations, but detail rows are unavailable. Poller snapshot is stale or failing."
                );
                if let Some(issue) = &self.fallback_issue {
                    message.push(' ');
                    message.push_str(issue);
                }
                Paragraph::new(message)
                    .style(crate::tui_theme::text_warning(&tp))
                    .render(
                        Rect::new(
                            inner.x,
                            inner.y.saturating_add(1),
                            inner.width,
                            inner.height.saturating_sub(1),
                        ),
                        frame,
                    );
            } else if let Some(issue) = &self.fallback_issue {
                Paragraph::new(issue.as_str())
                    .style(crate::tui_theme::text_warning(&tp))
                    .render(
                        Rect::new(
                            inner.x,
                            inner.y.saturating_add(1),
                            inner.width,
                            inner.height.saturating_sub(1),
                        ),
                        frame,
                    );
            } else {
                let hint = if self.show_released {
                    "No reservations match current filters. Press 'x' to toggle released."
                } else {
                    "Use `file_reservation_paths` to reserve files. Press 'r' to refresh."
                };
                crate::tui_panel_helpers::render_empty_state(
                    frame,
                    Rect::new(
                        inner.x,
                        inner.y.saturating_add(1),
                        inner.width,
                        inner.height.saturating_sub(1),
                    ),
                    "\u{1f512}",
                    "No Active Reservations",
                    hint,
                );
            }
        }

        // ── Footer summary ─────────────────────────────────────────────
        if footer_h > 0 {
            let footer_area = Rect::new(area.x, y, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }

        // ── Side detail panel (Lg+, not when modal active) ────────────
        if split.rects.len() >= 2 && show_side_detail {
            self.render_reservation_detail_panel(frame, split.rects[1]);
        }

        if let Some(form) = &self.create_form {
            render_reservation_create_modal(frame, area, form);
        }

        match self.preset_dialog_mode {
            PresetDialogMode::Save => render_save_preset_dialog(
                frame,
                area,
                &self.save_preset_name,
                &self.save_preset_description,
                self.save_preset_field,
            ),
            PresetDialogMode::Load => {
                let names = self.preset_names();
                render_load_preset_dialog(frame, area, &names, self.load_preset_cursor);
            }
            PresetDialogMode::None => {}
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
                key: "n",
                action: "Open create reservation form",
            },
            HelpEntry {
                key: "Ctrl+S",
                action: "Save current filter preset",
            },
            HelpEntry {
                key: "Ctrl+L",
                action: "Load preset list",
            },
            HelpEntry {
                key: "Delete",
                action: "Delete selected preset (load dialog)",
            },
            HelpEntry {
                key: ".",
                action: "Open actions (single or batch)",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail panel",
            },
            HelpEntry {
                key: "Mouse",
                action: "Wheel/Click navigate rows",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "File reservations held by agents. Press n to create; Space/v/A/C for multi-select; Ctrl+S/Ctrl+L for presets.",
        )
    }

    fn consumes_text_input(&self) -> bool {
        self.create_form.is_some()
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
            .gauge_style(Style::default().fg(tp.text_primary).bg(gauge_bg));
        if ttl_width >= 12 {
            gauge = gauge.label(&row.label);
        }
        gauge.render(Rect::new(ttl_x, y, ttl_width, 1), frame);
    }
}

fn render_create_form_label(
    frame: &mut Frame<'_>,
    inner: Rect,
    cursor_y: &mut u16,
    bottom: u16,
    label: &str,
    focused: bool,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    if *cursor_y >= bottom {
        return;
    }
    let style = if focused {
        Style::default().fg(tp.selection_indicator).bold()
    } else {
        crate::tui_theme::text_meta(tp)
    };
    Paragraph::new(label.to_string())
        .style(style)
        .render(Rect::new(inner.x, *cursor_y, inner.width, 1), frame);
    *cursor_y = (*cursor_y).saturating_add(1);
}

fn render_create_form_error(
    frame: &mut Frame<'_>,
    inner: Rect,
    cursor_y: &mut u16,
    bottom: u16,
    error: Option<&str>,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    if let Some(error) = error
        && *cursor_y < bottom
    {
        Paragraph::new(error.to_string())
            .style(crate::tui_theme::text_warning(tp))
            .render(Rect::new(inner.x, *cursor_y, inner.width, 1), frame);
        *cursor_y = (*cursor_y).saturating_add(1);
    }
}

#[must_use]
fn reservation_create_modal_rect(area: Rect) -> Rect {
    if area.width < 46 || area.height < 16 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let modal_width = ((u32::from(area.width) * 86) / 100).clamp(62, 110) as u16;
    let modal_height = ((u32::from(area.height) * 86) / 100).clamp(18, 34) as u16;
    let width = modal_width.min(area.width.saturating_sub(2));
    let height = modal_height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

#[allow(clippy::too_many_lines)]
fn render_reservation_create_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &ReservationCreateFormState,
) {
    if area.width < 46 || area.height < 16 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay))
        .render(area, frame);

    let modal = reservation_create_modal_rect(area);
    let title = "Create Reservation";
    let block = Block::default()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.selection_indicator));
    let inner = block.inner(modal);
    block.render(modal, frame);
    if inner.height < 10 || inner.width < 18 {
        return;
    }

    let mut cursor_y = inner.y;
    let bottom = inner.y + inner.height;

    let project_label = if form.project_slug.is_empty() {
        "(unknown)"
    } else {
        &form.project_slug
    };
    let agent_label = if form.agent_name.is_empty() {
        "(unknown)"
    } else {
        &form.agent_name
    };
    Paragraph::new(format!("Project: {project_label}"))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);
    Paragraph::new(format!("Agent:   {agent_label}"))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);

    render_create_form_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "Paths* (one glob per line)",
        matches!(form.focus, ReservationCreateField::Paths),
        &tp,
    );
    let suggested_rows = if inner.height >= 24 {
        7
    } else if inner.height >= 20 {
        6
    } else {
        RESERVATION_PATH_MIN_ROWS
    };
    let max_rows = bottom.saturating_sub(cursor_y).saturating_sub(7);
    let path_rows = suggested_rows.min(max_rows.max(1));
    if cursor_y < bottom && path_rows > 0 {
        ftui_widgets::Widget::render(
            &form.paths_input,
            Rect::new(inner.x, cursor_y, inner.width, path_rows),
            frame,
        );
        cursor_y = cursor_y.saturating_add(path_rows);
    }
    render_create_form_error(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.paths.as_deref(),
        &tp,
    );

    if cursor_y < bottom {
        let exclusive = if form.exclusive {
            "Exclusive: [x]"
        } else {
            "Exclusive: [ ]"
        };
        let style = if matches!(form.focus, ReservationCreateField::Exclusive) {
            Style::default().fg(tp.selection_indicator).bold()
        } else {
            Style::default().fg(tp.text_secondary)
        };
        Paragraph::new(exclusive)
            .style(style)
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }

    if cursor_y < bottom {
        let ttl_options = RESERVATION_TTL_PRESETS
            .iter()
            .enumerate()
            .map(|(idx, (label, _))| {
                if idx == form.ttl_idx {
                    format!("[{label}]")
                } else {
                    label.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let ttl_line = format!("TTL: {ttl_options}");
        let style = if matches!(form.focus, ReservationCreateField::Ttl) {
            Style::default().fg(tp.selection_indicator).bold()
        } else {
            Style::default().fg(tp.text_secondary)
        };
        Paragraph::new(ttl_line)
            .style(style)
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    if form.custom_ttl_enabled() {
        render_create_form_label(
            frame,
            inner,
            &mut cursor_y,
            bottom,
            "Custom TTL* (Nh or Nm)",
            matches!(form.focus, ReservationCreateField::CustomTtl),
            &tp,
        );
        if cursor_y < bottom {
            form.custom_ttl_input
                .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
            cursor_y = cursor_y.saturating_add(1);
        }
    }
    render_create_form_error(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.ttl.as_deref(),
        &tp,
    );

    render_create_form_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "Reason (optional)",
        matches!(form.focus, ReservationCreateField::Reason),
        &tp,
    );
    if cursor_y < bottom {
        form.reason_input
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }

    render_create_form_error(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.general.as_deref(),
        &tp,
    );

    let footer_y = bottom.saturating_sub(1);
    let footer = "Tab/Shift+Tab fields • ←/→ TTL • Space toggle exclusive • F5/Ctrl+Enter submit • Esc cancel";
    Paragraph::new(footer)
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, footer_y, inner.width, 1), frame);
}

fn centered_overlay_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.clamp(24, area.width.saturating_sub(2));
    let height = height.clamp(6, area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn render_save_preset_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &str,
    description: &str,
    active_field: SavePresetField,
) {
    if area.width < 36 || area.height < 8 {
        return;
    }
    let overlay = centered_overlay_rect(area, 64, 9);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Save Reservation Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay));
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    if inner.height == 0 {
        return;
    }
    let name_marker = if active_field == SavePresetField::Name {
        ">"
    } else {
        " "
    };
    let desc_marker = if active_field == SavePresetField::Description {
        ">"
    } else {
        " "
    };
    let description = if description.is_empty() {
        "<optional>".to_string()
    } else {
        description.to_string()
    };
    let lines = vec![
        ftui::text::Line::from(ftui::text::Span::styled(
            "Enter to save · Tab to switch field · Esc to cancel",
            crate::tui_theme::text_meta(&tp),
        )),
        ftui::text::Line::from(ftui::text::Span::raw(format!("{name_marker} Name: {name}"))),
        ftui::text::Line::from(ftui::text::Span::raw(format!(
            "{desc_marker} Description: {description}"
        ))),
    ];
    Paragraph::new(ftui::text::Text::from_lines(lines)).render(inner, frame);
}

fn render_load_preset_dialog(frame: &mut Frame<'_>, area: Rect, names: &[String], cursor: usize) {
    if area.width < 36 || area.height < 8 {
        return;
    }
    let overlay = centered_overlay_rect(area, 64, 12);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Load Reservation Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay));
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    if inner.height == 0 {
        return;
    }
    let mut lines = vec![ftui::text::Line::from(ftui::text::Span::styled(
        "Enter apply · Del delete · j/k move · Esc cancel",
        crate::tui_theme::text_meta(&tp),
    ))];
    if names.is_empty() {
        lines.push(ftui::text::Line::from(ftui::text::Span::styled(
            "No saved presets for Reservations.",
            crate::tui_theme::text_warning(&tp),
        )));
    } else {
        let visible_rows = usize::from(inner.height.saturating_sub(2)).max(1);
        let start = cursor.saturating_sub(visible_rows.saturating_sub(1));
        let end = (start + visible_rows).min(names.len());
        for (idx, name) in names.iter().enumerate().take(end).skip(start) {
            let marker = if idx == cursor {
                crate::tui_theme::SELECTION_PREFIX
            } else {
                crate::tui_theme::SELECTION_PREFIX_EMPTY
            };
            lines.push(ftui::text::Line::from(ftui::text::Span::raw(format!(
                "{marker}{name}"
            ))));
        }
    }
    Paragraph::new(ftui::text::Text::from_lines(lines)).render(inner, frame);
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

const fn trend_for(current: u64, previous: u64) -> MetricTrend {
    if current > previous {
        MetricTrend::Up
    } else if current < previous {
        MetricTrend::Down
    } else {
        MetricTrend::Flat
    }
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
        // The full fallback message wraps across lines in the bordered block,
        // so check for a substring that fits on a single rendered row.
        assert!(
            text.contains("DB snapshots"),
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
            text.contains("No Active Reservations"),
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
        assert!(bindings.iter().any(|b| b.key == "n"));
        assert!(bindings.iter().any(|b| b.key == "Ctrl+S"));
        assert!(bindings.iter().any(|b| b.key == "Ctrl+L"));
        assert!(bindings.iter().any(|b| b.key == "."));
        assert_eq!(
            screen.context_help_tip(),
            Some(
                "File reservations held by agents. Press n to create; Space/v/A/C for multi-select; Ctrl+S/Ctrl+L for presets.",
            )
        );
    }

    #[test]
    fn n_opens_create_form_and_escape_closes_it() {
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
        screen.sorted_keys.push(key);
        screen.table_state.selected = Some(0);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('n'))), &state);
        assert!(screen.create_form.is_some());
        assert!(screen.consumes_text_input());

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert!(screen.create_form.is_none());
        assert!(!screen.consumes_text_input());
    }

    #[test]
    fn parse_custom_ttl_seconds_accepts_minutes_and_hours() {
        assert_eq!(
            ReservationsScreen::parse_custom_ttl_seconds("90m").unwrap(),
            5400
        );
        assert_eq!(
            ReservationsScreen::parse_custom_ttl_seconds("2h").unwrap(),
            7200
        );
        assert_eq!(
            ReservationsScreen::parse_custom_ttl_seconds(" 3H ").unwrap(),
            10_800
        );
        assert!(ReservationsScreen::parse_custom_ttl_seconds("0m").is_err());
        assert!(ReservationsScreen::parse_custom_ttl_seconds("15s").is_err());
    }

    #[test]
    fn validate_create_form_rejects_missing_context_and_paths() {
        let form = ReservationCreateFormState::new(String::new(), String::new());
        let errors = match ReservationsScreen::validate_create_form(&form) {
            Ok(payload) => panic!(
                "expected validation error, got payload: {:?}",
                payload.paths
            ),
            Err(errors) => errors,
        };
        assert!(
            errors
                .paths
                .as_deref()
                .is_some_and(|msg| msg.contains("at least one")),
            "expected paths error, got: {:?}",
            errors.paths
        );
        assert!(
            errors
                .general
                .as_deref()
                .is_some_and(|msg| msg.contains("infer project/agent")),
            "expected context inference error, got: {:?}",
            errors.general
        );
    }

    #[test]
    fn validate_create_form_accepts_custom_ttl_reason_and_paths() {
        let mut form = ReservationCreateFormState::new("proj".to_string(), "BlueLake".to_string());
        form.paths_input.set_text("src/**\n tests/**\n");
        form.ttl_idx = RESERVATION_TTL_CUSTOM_INDEX;
        form.custom_ttl_input.set_value("2h");
        form.reason_input.set_value("br-3oavg");

        let payload = ReservationsScreen::validate_create_form(&form).expect("valid payload");
        assert_eq!(payload.project_key, "proj");
        assert_eq!(payload.agent_name, "BlueLake");
        assert_eq!(
            payload.paths,
            vec!["src/**".to_string(), "tests/**".to_string()]
        );
        assert_eq!(payload.ttl_seconds, 7200);
        assert!(payload.exclusive);
        assert_eq!(payload.reason.as_deref(), Some("br-3oavg"));
    }

    #[test]
    fn submit_create_form_keeps_modal_open_when_validation_fails() {
        let state = test_state();
        let mut screen = ReservationsScreen::new();
        screen.create_form = Some(ReservationCreateFormState::new(
            String::new(),
            String::new(),
        ));

        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::F(5))), &state);
        assert!(matches!(cmd, Cmd::None));
        let form = screen
            .create_form
            .as_ref()
            .expect("validation failure should keep create form open");
        assert!(form.errors.paths.is_some());
        assert!(form.errors.general.is_some());
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
    fn reservations_presets_save_load_delete_lifecycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = ReservationsScreen::with_filter_presets_path_for_test(&path);

        screen.sort_col = COL_AGENT;
        screen.sort_asc = false;
        screen.show_released = true;
        assert!(screen.save_named_preset("triage", Some("desc".to_string())));

        let loaded = crate::tui_persist::load_screen_filter_presets(&path).expect("load presets");
        let preset = loaded
            .get(RESERVATIONS_PRESET_SCREEN_ID, "triage")
            .expect("saved preset");
        assert_eq!(preset.values.get("sort_col").map(String::as_str), Some("0"));
        assert_eq!(
            preset.values.get("sort_asc").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            preset.values.get("show_released").map(String::as_str),
            Some("true")
        );

        screen.sort_col = COL_TTL;
        screen.sort_asc = true;
        screen.show_released = false;
        assert!(screen.apply_named_preset("triage"));
        assert_eq!(screen.sort_col, COL_AGENT);
        assert!(!screen.sort_asc);
        assert!(screen.show_released);

        assert!(screen.remove_named_preset("triage"));
        assert!(screen.preset_names().is_empty());
    }

    #[test]
    fn ctrl_shortcuts_drive_reservation_preset_dialog_flow() {
        let state = test_state();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = ReservationsScreen::with_filter_presets_path_for_test(&path);

        let ctrl_s = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('s'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::CTRL,
        });
        screen.update(&ctrl_s, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Save);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Enter)), &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
        assert!(!screen.preset_names().is_empty());

        let ctrl_l = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('l'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::CTRL,
        });
        screen.update(&ctrl_l, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Load);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Delete)), &state);
        assert!(screen.preset_names().is_empty());
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
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
