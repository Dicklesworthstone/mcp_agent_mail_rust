//! Portable screen contract and public-replay screens used by the browser shell.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use ftui::Event;
use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, Style};
use ftui_runtime::program::Cmd;

use crate::state::{ReplayEventCursor, TuiSharedState};
use crate::tui_events::MailEvent;

pub use crate::tui_screen_registry::*;

#[derive(Debug, Clone)]
pub struct HelpEntry {
    pub key: &'static str,
    pub action: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataGeneration {
    pub event_epoch: u64,
    pub event_total_pushed: u64,
    pub console_log_seq: u64,
    pub db_stats_gen: u64,
    pub request_gen: u64,
}

impl DataGeneration {
    #[must_use]
    pub const fn stale() -> Self {
        Self {
            event_epoch: u64::MAX,
            event_total_pushed: u64::MAX,
            console_log_seq: u64::MAX,
            db_stats_gen: u64::MAX,
            request_gen: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct DirtyFlags {
    pub events: bool,
    pub console_log: bool,
    pub db_stats: bool,
    pub requests: bool,
}

impl DirtyFlags {
    #[must_use]
    pub const fn any(self) -> bool {
        self.events || self.console_log || self.db_stats || self.requests
    }
}

#[must_use]
pub const fn dirty_since(previous: &DataGeneration, current: &DataGeneration) -> DirtyFlags {
    DirtyFlags {
        events: current.event_epoch != previous.event_epoch
            || current.event_total_pushed != previous.event_total_pushed,
        console_log: current.console_log_seq != previous.console_log_seq,
        db_stats: current.db_stats_gen != previous.db_stats_gen,
        requests: current.request_gen != previous.request_gen,
    }
}

#[derive(Debug, Clone)]
pub enum MailScreenMsg {
    Noop,
    DeepLink(DeepLinkTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLinkTarget {
    TimelineAtTime(i64),
    SearchFocused(String),
}

pub trait MailScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg>;
    fn view(&self, frame: &mut ftui::Frame<'_>, area: Rect, state: &TuiSharedState);
    fn tick(&mut self, _tick_count: u64, _state: &TuiSharedState) {}
    fn prefers_fast_tick(&self, _state: &TuiSharedState) -> bool {
        false
    }
    fn keybindings(&self) -> Vec<HelpEntry> {
        Vec::new()
    }
    fn context_help_tip(&self) -> Option<&'static str> {
        None
    }
    fn consumes_text_input(&self) -> bool {
        false
    }
    fn title(&self) -> &'static str {
        "Dashboard"
    }
    fn tab_label(&self) -> &'static str {
        self.title()
    }
}

#[must_use]
pub fn contains_ci(text: &str, query: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

/// Browser-safe read-only screen used for registry entries whose native
/// implementations depend on the database, filesystem, or server runtime.
/// Every row is derived from the validated public replay pack, and mouse/keys
/// operate on the same rendered list rectangle cached during `view()`.
pub struct PublicReplayScreen {
    list_area: Cell<Rect>,
    search_input_area: Cell<Rect>,
    // The App view contract is `&self`, so these remain interior-mutable: a
    // resize changes the rendered page height during view and must immediately
    // reconcile selection/scroll with the geometry used for mouse hit testing.
    selected: Cell<usize>,
    scroll: Cell<usize>,
    search_query: String,
    search_active: bool,
    row_cache: RefCell<Option<PublicRowsCache>>,
}

#[derive(Debug, Clone)]
struct PublicRowsCache {
    screen: MailScreenId,
    generation: DataGeneration,
    query: String,
    rows: Vec<String>,
    event_cursor: Option<ReplayEventCursor>,
}

impl Default for PublicReplayScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicReplayScreen {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            list_area: Cell::new(Rect::new(0, 0, 0, 0)),
            search_input_area: Cell::new(Rect::new(0, 0, 0, 0)),
            selected: Cell::new(0),
            scroll: Cell::new(0),
            search_query: String::new(),
            search_active: false,
            row_cache: RefCell::new(None),
        }
    }

    pub fn reset(&mut self) {
        self.selected.set(0);
        self.scroll.set(0);
        self.search_query.clear();
        self.search_active = false;
        self.list_area.set(Rect::new(0, 0, 0, 0));
        self.search_input_area.set(Rect::new(0, 0, 0, 0));
        *self.row_cache.get_mut() = None;
    }

    pub fn begin_search(&mut self, query: &str) {
        self.search_query = query.chars().take(96).collect();
        self.search_active = true;
        self.selected.set(0);
        self.scroll.set(0);
        *self.row_cache.get_mut() = None;
    }

    /// Reactivate the existing Search editor without changing its query or
    /// list position. This is the keyboard equivalent of clicking the visible
    /// Search prompt after leaving edit mode with Enter or Escape.
    pub fn reactivate_search(&mut self) -> bool {
        if self.search_active {
            return false;
        }
        self.search_active = true;
        true
    }

    #[must_use]
    pub fn consumes_text_input(&self, screen: MailScreenId) -> bool {
        screen == MailScreenId::Search && self.search_active
    }

    #[must_use]
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    #[must_use]
    pub const fn selected_row(&self) -> usize {
        self.selected.get()
    }

    /// Focus the public timeline row whose event timestamp is closest to the
    /// production Dashboard deep-link target.
    pub fn focus_timeline_at(&mut self, timestamp_micros: i64, state: &TuiSharedState) -> bool {
        let events = state.replay_events();
        let Some(index) = events
            .iter()
            .filter(|event| event_visible_on(MailScreenId::Timeline, event))
            .enumerate()
            .min_by_key(|(_, event)| event.timestamp_micros().abs_diff(timestamp_micros))
            .map(|(index, _)| index)
        else {
            return false;
        };
        let previous = (self.selected.get(), self.scroll.get());
        self.selected.set(index);
        let page = usize::from(self.list_area.get().height.max(1));
        self.scroll.set(index.saturating_sub(page / 2));
        previous != (self.selected.get(), self.scroll.get())
    }

    fn ensure_rows(&self, screen: MailScreenId, state: &TuiSharedState) {
        if uses_direct_event_rows(screen) {
            let previous_cursor = self
                .row_cache
                .borrow()
                .as_ref()
                .filter(|cache| cache.screen == screen && cache.query == self.search_query)
                .and_then(|cache| cache.event_cursor);
            let batch = state.replay_event_batch(previous_cursor);
            // Build the generation from the same locked ring snapshot as the
            // returned event batch. This avoids both a redundant full-ring
            // generation read and a check-then-read race with reset/push.
            let generation = public_rows_generation(
                screen,
                DataGeneration {
                    event_epoch: batch.cursor.epoch,
                    event_total_pushed: batch.cursor.total_pushed,
                    ..DataGeneration::default()
                },
            );

            if batch.incremental {
                let normalized_query = (screen == MailScreenId::Search
                    && !self.search_query.is_empty())
                .then(|| self.search_query.to_ascii_lowercase());
                let mut cache = self.row_cache.borrow_mut();
                let cache = cache
                    .as_mut()
                    .expect("an incremental cursor must belong to a materialized row cache");
                for event in batch.events {
                    if event_visible_on(screen, &event) {
                        let row = public_event_row(&event);
                        if normalized_query
                            .as_ref()
                            .is_none_or(|query| row.to_ascii_lowercase().contains(query))
                        {
                            cache.rows.push(row);
                        }
                    }
                }
                cache.generation = generation;
                cache.event_cursor = Some(batch.cursor);
                return;
            }

            let mut rows = public_event_rows_from_events(screen, batch.events);
            filter_search_rows(screen, &self.search_query, &mut rows);
            *self.row_cache.borrow_mut() = Some(PublicRowsCache {
                screen,
                generation,
                query: self.search_query.clone(),
                rows,
                event_cursor: Some(batch.cursor),
            });
            return;
        }

        // Public projections intentionally depend on only a subset of the
        // replay channels. Mask unrelated generations so (for example) a
        // request-counter tick cannot rematerialize event-derived rows.
        let generation = public_rows_generation(screen, state.data_generation());
        let needs_rebuild = self.row_cache.borrow().as_ref().is_none_or(|cache| {
            cache.screen != screen
                || cache.generation != generation
                || cache.query != self.search_query
        });
        if !needs_rebuild {
            return;
        }

        let mut rows = public_rows(screen, state);
        filter_search_rows(screen, &self.search_query, &mut rows);
        *self.row_cache.borrow_mut() = Some(PublicRowsCache {
            screen,
            generation,
            query: self.search_query.clone(),
            rows,
            event_cursor: None,
        });
    }

    fn visible_row_count(&self, screen: MailScreenId, state: &TuiSharedState) -> usize {
        self.ensure_rows(screen, state);
        self.row_cache
            .borrow()
            .as_ref()
            .map_or(0, |cache| cache.rows.len())
    }

    /// Reconcile preserved selection/scroll state with freshly reset replay
    /// data so a shorter result set cannot leave the list scrolled past EOF.
    pub fn normalize(&mut self, screen: MailScreenId, state: &TuiSharedState) {
        let row_count = self.visible_row_count(screen, state);
        let page = usize::from(self.list_area.get().height.max(1));
        self.clamp_to_rows(row_count, page);
    }

    fn clamp_to_rows(&self, row_count: usize, page: usize) {
        if row_count == 0 {
            self.selected.set(0);
            self.scroll.set(0);
            return;
        }
        let selected = self.selected.get().min(row_count - 1);
        let mut scroll = self.scroll.get();
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll.saturating_add(page) {
            scroll = selected + 1 - page;
        }
        self.selected.set(selected);
        // Keep the final page full whenever enough rows remain. Clamping only
        // to `row_count - 1` can strand the viewport on a one-row final page
        // after a preserved scroll position outlives a data/geometry change.
        self.scroll
            .set(scroll.min(row_count.saturating_sub(page.max(1))));
    }

    /// Process list navigation. Returns true when visible selection or scroll
    /// state changed, allowing the host status contract to expose interaction.
    pub fn update(&mut self, event: &Event, screen: MailScreenId, state: &TuiSharedState) -> bool {
        let previous = (
            self.selected.get(),
            self.scroll.get(),
            self.search_query.clone(),
            self.search_active,
        );
        let page = usize::from(self.list_area.get().height.max(1));
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && screen == MailScreenId::Search
                    && self.search_active =>
            {
                match key.code {
                    KeyCode::Escape | KeyCode::Enter => self.search_active = false,
                    KeyCode::Backspace => {
                        self.search_query.pop();
                        self.selected.set(0);
                        self.scroll.set(0);
                    }
                    KeyCode::Char(character)
                        if !character.is_control()
                            && !key.modifiers.intersects(
                                Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER,
                            ) =>
                    {
                        if self.search_query.chars().count() < 96 {
                            self.search_query.push(character);
                            self.selected.set(0);
                            self.scroll.set(0);
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(paste) if screen == MailScreenId::Search && self.search_active => {
                let remaining = 96_usize.saturating_sub(self.search_query.chars().count());
                self.search_query.extend(paste.text.chars().take(remaining));
                self.selected.set(0);
                self.scroll.set(0);
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected.set(self.selected.get().saturating_add(1));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected.set(self.selected.get().saturating_sub(1));
                }
                KeyCode::PageDown => {
                    self.selected.set(self.selected.get().saturating_add(page));
                }
                KeyCode::PageUp => {
                    self.selected.set(self.selected.get().saturating_sub(page));
                }
                KeyCode::Home => self.selected.set(0),
                KeyCode::End => self.selected.set(usize::MAX),
                _ => return false,
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if screen == MailScreenId::Search
                        && crate::tui_hit_regions::point_in_rect(
                            self.search_input_area.get(),
                            mouse.x,
                            mouse.y,
                        ) =>
                {
                    self.search_active = true;
                }
                MouseEventKind::Down(MouseButton::Left)
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    let relative = usize::from(mouse.y.saturating_sub(self.list_area.get().y));
                    let remaining_rows = self
                        .visible_row_count(screen, state)
                        .saturating_sub(self.scroll.get());
                    if relative >= remaining_rows {
                        return false;
                    }
                    self.selected
                        .set(self.scroll.get().saturating_add(relative));
                }
                MouseEventKind::ScrollDown
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    self.selected.set(self.selected.get().saturating_add(3));
                }
                MouseEventKind::ScrollUp
                    if crate::tui_hit_regions::point_in_rect(
                        self.list_area.get(),
                        mouse.x,
                        mouse.y,
                    ) =>
                {
                    self.selected.set(self.selected.get().saturating_sub(3));
                }
                _ => return false,
            },
            _ => return false,
        }
        let row_count = self.visible_row_count(screen, state);
        self.clamp_to_rows(row_count, page);
        (
            self.selected.get(),
            self.scroll.get(),
            self.search_query.clone(),
            self.search_active,
        ) != previous
    }

    pub fn view(
        &self,
        screen: MailScreenId,
        frame: &mut ftui::Frame<'_>,
        area: Rect,
        state: &TuiSharedState,
    ) {
        let palette = crate::tui_theme::TuiThemePalette::current();
        let meta = screen_meta(screen);
        self.ensure_rows(screen, state);
        let row_cache = self.row_cache.borrow();
        let rows = &row_cache
            .as_ref()
            .expect("public replay rows must be cached before rendering")
            .rows;

        let header_height = area.height.min(3);
        let header_area = Rect::new(area.x, area.y, area.width, header_height);
        self.search_input_area
            .set(search_input_hit_area(screen, header_area));
        let body_area = Rect::new(
            area.x,
            area.y.saturating_add(header_height),
            area.width,
            area.height.saturating_sub(header_height),
        );
        Paragraph::new(Text::from_lines([
            Line::from_spans([
                Span::styled(
                    format!(" {} ", meta.title),
                    Style::default().fg(palette.status_accent).bold(),
                ),
                Span::styled(
                    "PUBLIC REPLAY · READ-ONLY",
                    Style::default().fg(palette.severity_ok).bold(),
                ),
            ]),
            Line::from_spans([Span::styled(
                browser_projection_description(screen),
                Style::default().fg(palette.text_secondary),
            )]),
            Line::from_spans([Span::styled(
                if screen == MailScreenId::Search {
                    format!(
                        "Live Search: {}{}",
                        if self.search_query.is_empty() {
                            "type to filter events".to_string()
                        } else {
                            self.search_query.clone()
                        },
                        if self.search_active {
                            " ▌"
                        } else {
                            " · click or press / to edit"
                        }
                    )
                } else {
                    "Click rows or use ↑/↓, j/k, PageUp/PageDown · click any top tab to switch screens".to_string()
                },
                Style::default().fg(palette.text_muted),
            )]),
        ]))
        .style(Style::default().bg(palette.bg_deep))
        .render(header_area, frame);

        if body_area.height == 0 {
            self.list_area.set(Rect::new(0, 0, 0, 0));
            return;
        }
        let record_noun = if rows.len() == 1 { "record" } else { "records" };
        let title = format!(
            " {} {record_noun} · sanitized public demo details ",
            rows.len()
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(&title)
            .style(
                Style::default()
                    .fg(palette.panel_border)
                    .bg(palette.panel_bg),
            );
        let inner = block.inner(body_area);
        block.render(body_area, frame);
        self.list_area.set(inner);
        self.clamp_to_rows(rows.len(), usize::from(inner.height.max(1)));
        let selected = self.selected.get();
        let scroll = self.scroll.get();

        let lines = rows
            .iter()
            .enumerate()
            .skip(scroll)
            .take(usize::from(inner.height))
            .map(|(index, row)| {
                let row_selected = index == selected;
                let indicator = if row_selected { "▶" } else { " " };
                let style = if row_selected {
                    Style::default()
                        .fg(palette.selection_fg)
                        .bg(palette.selection_bg)
                        .bold()
                } else if index % 2 == 1 {
                    Style::default()
                        .fg(palette.text_primary)
                        .bg(palette.table_row_alt_bg)
                } else {
                    Style::default()
                        .fg(palette.text_primary)
                        .bg(palette.panel_bg)
                };
                Line::from_spans([Span::styled(format!("{indicator} {row}"), style)])
            })
            .collect::<Vec<_>>();
        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }
}

const fn search_input_hit_area(screen: MailScreenId, header_area: Rect) -> Rect {
    // The editable prompt is the third rendered header line. At one- or
    // two-row heights that line is clipped, so the visible title/description
    // must not become an invisible Search activation target.
    if matches!(screen, MailScreenId::Search) && header_area.height >= 3 {
        Rect::new(
            header_area.x,
            header_area.y.saturating_add(2),
            header_area.width,
            1,
        )
    } else {
        Rect::new(0, 0, 0, 0)
    }
}

const fn public_rows_generation(
    screen: MailScreenId,
    generation: DataGeneration,
) -> DataGeneration {
    match screen {
        MailScreenId::Messages
        | MailScreenId::Threads
        | MailScreenId::ToolMetrics
        | MailScreenId::Search
        | MailScreenId::Timeline
        | MailScreenId::Explorer => DataGeneration {
            event_epoch: generation.event_epoch,
            event_total_pushed: generation.event_total_pushed,
            console_log_seq: 0,
            db_stats_gen: 0,
            request_gen: 0,
        },
        MailScreenId::Analytics => DataGeneration {
            event_epoch: generation.event_epoch,
            event_total_pushed: generation.event_total_pushed,
            console_log_seq: 0,
            db_stats_gen: 0,
            request_gen: generation.request_gen,
        },
        MailScreenId::Agents
        | MailScreenId::Reservations
        | MailScreenId::Projects
        | MailScreenId::Contacts
        | MailScreenId::ArchiveBrowser
        | MailScreenId::Atc => DataGeneration {
            event_epoch: 0,
            event_total_pushed: 0,
            console_log_seq: 0,
            db_stats_gen: generation.db_stats_gen,
            request_gen: 0,
        },
        MailScreenId::SystemHealth => DataGeneration {
            event_epoch: 0,
            event_total_pushed: 0,
            console_log_seq: 0,
            db_stats_gen: generation.db_stats_gen,
            request_gen: generation.request_gen,
        },
        MailScreenId::Dashboard | MailScreenId::Attachments => DataGeneration {
            event_epoch: 0,
            event_total_pushed: 0,
            console_log_seq: 0,
            db_stats_gen: 0,
            request_gen: 0,
        },
    }
}

const fn browser_projection_description(screen: MailScreenId) -> &'static str {
    match screen {
        MailScreenId::Dashboard => {
            "Production DashboardScreen driven by the sanitized replay state."
        }
        MailScreenId::Messages => {
            "Synthetic public message events; mailbox reads and mutations are unavailable."
        }
        MailScreenId::Threads => "Synthetic public messages grouped by replay thread identifier.",
        MailScreenId::Agents => {
            "Synthetic public agent roster backed by aggregate-safe replay data."
        }
        MailScreenId::Search => "Client-side text filter across sanitized public replay events.",
        MailScreenId::Reservations => {
            "Synthetic reservation rows; live lock operations are unavailable."
        }
        MailScreenId::ToolMetrics => {
            "Synthetic tool, HTTP, and Git telemetry from the public replay."
        }
        MailScreenId::SystemHealth => {
            "Read-only aggregate health summary from the public replay state."
        }
        MailScreenId::Timeline => {
            "Chronological sanitized replay events with Dashboard deep-link focus."
        }
        MailScreenId::Projects => {
            "Synthetic project labels paired with count-only aggregate metrics."
        }
        MailScreenId::Contacts => {
            "Synthetic public contact graph rows; approval actions are unavailable."
        }
        MailScreenId::Explorer => "Read-only event explorer over the sanitized replay stream.",
        MailScreenId::Analytics => {
            "Derived summary of synthetic telemetry kinds and request counters."
        }
        MailScreenId::Attachments => {
            "The public replay deliberately contains no attachment payload metadata."
        }
        MailScreenId::ArchiveBrowser => {
            "Released synthetic reservations only; Git/archive payloads are not shipped."
        }
        MailScreenId::Atc => {
            "Synthetic coordination roster only; no readiness or authority decision is asserted."
        }
    }
}

fn public_rows(screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
    match screen {
        MailScreenId::Messages => public_event_rows(screen, state),
        MailScreenId::Threads => {
            let mut threads: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
            for event in state.replay_events() {
                let (thread_id, subject, project) = match event {
                    MailEvent::MessageSent {
                        thread_id,
                        subject,
                        project,
                        ..
                    }
                    | MailEvent::MessageReceived {
                        thread_id,
                        subject,
                        project,
                        ..
                    } => (thread_id, subject, project),
                    _ => continue,
                };
                let entry = threads
                    .entry(thread_id)
                    .or_insert_with(|| (subject, project, 0));
                entry.2 = entry.2.saturating_add(1);
            }
            threads
                .into_iter()
                .map(|(thread, (subject, project, count))| {
                    format!("◉ {thread:<26} messages:{count:<4} {subject} · {project}")
                })
                .collect()
        }
        MailScreenId::Attachments => vec![
            "No attachment metadata or payload bytes are included in this public replay."
                .to_string(),
        ],
        MailScreenId::Agents => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .agents_list
            .into_iter()
            .map(|agent| {
                format!(
                    "● {:<20} {:<14} {:<16} {}",
                    agent.name, agent.program, agent.model, agent.project
                )
            })
            .collect(),
        MailScreenId::Projects => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .projects_list
            .into_iter()
            .map(|project| {
                format!(
                    "▣ {:<26} agents:{:<4} messages:{:<7} locks:{:<4}",
                    project.slug,
                    project.agent_count,
                    project.message_count,
                    project.reservation_count
                )
            })
            .collect(),
        MailScreenId::Contacts => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .contacts_list
            .into_iter()
            .map(|contact| {
                format!(
                    "◉ {:<18} → {:<18} {:<10} {}",
                    contact.from_agent, contact.to_agent, contact.status, contact.reason
                )
            })
            .collect(),
        MailScreenId::Reservations => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .reservation_snapshots
            .into_iter()
            .map(|reservation| {
                format!(
                    "{} {:<18} {:<26} {}",
                    if reservation.exclusive { "X" } else { "S" },
                    reservation.agent_name,
                    reservation.project_slug,
                    reservation.path_pattern
                )
            })
            .collect(),
        MailScreenId::ArchiveBrowser => {
            let rows = state
                .db_stats_snapshot()
                .unwrap_or_default()
                .reservation_snapshots
                .into_iter()
                .filter(|reservation| reservation.released_ts.is_some())
                .map(|reservation| {
                    format!(
                        "released {:<18} {:<26} {}",
                        reservation.agent_name, reservation.project_slug, reservation.path_pattern
                    )
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![
                    "No released reservation records are present in this replay phase.".to_string(),
                ]
            } else {
                rows
            }
        }
        MailScreenId::SystemHealth => {
            let stats = state.db_stats_snapshot().unwrap_or_default();
            vec![
                format!(
                    "Database snapshot     projects:{:<5} agents:{:<6} messages:{}",
                    stats.projects, stats.agents, stats.messages
                ),
                format!("Reservation ledger    active:{}", stats.file_reservations),
                format!("Contact graph         links:{}", stats.contact_links),
                format!("Acknowledgements      pending:{}", stats.ack_pending),
                format!("Transport             {}", state.transport_mode_label()),
                format!("Request latency       avg:{}ms", state.avg_latency_ms()),
            ]
        }
        MailScreenId::Atc => state
            .db_stats_snapshot()
            .unwrap_or_default()
            .agents_list
            .into_iter()
            .take(200)
            .map(|agent| {
                format!(
                    "● {:<20} {:<22} {:<14} {}",
                    agent.name, agent.project, agent.program, agent.model
                )
            })
            .collect(),
        MailScreenId::Analytics => {
            let mut counts = BTreeMap::<&'static str, usize>::new();
            for event in state.replay_events() {
                if event_visible_on(screen, &event) {
                    let label = event.kind().compact_label();
                    *counts.entry(label).or_default() += 1;
                }
            }
            let requests = state.request_counters();
            let mut rows = counts
                .into_iter()
                .map(|(kind, count)| format!("telemetry {:<18} events:{count}", kind))
                .collect::<Vec<_>>();
            rows.push(format!(
                "requests total:{} 2xx:{} 4xx:{} 5xx:{} avg:{}ms",
                requests.total,
                requests.status_2xx,
                requests.status_4xx,
                requests.status_5xx,
                state.avg_latency_ms()
            ));
            rows
        }
        MailScreenId::ToolMetrics
        | MailScreenId::Search
        | MailScreenId::Timeline
        | MailScreenId::Explorer => public_event_rows(screen, state),
        MailScreenId::Dashboard => Vec::new(),
    }
}

const fn uses_direct_event_rows(screen: MailScreenId) -> bool {
    matches!(
        screen,
        MailScreenId::Messages
            | MailScreenId::ToolMetrics
            | MailScreenId::Search
            | MailScreenId::Timeline
            | MailScreenId::Explorer
    )
}

fn filter_search_rows(screen: MailScreenId, query: &str, rows: &mut Vec<String>) {
    if screen == MailScreenId::Search && !query.is_empty() {
        let normalized_query = query.to_ascii_lowercase();
        rows.retain(|row| row.to_ascii_lowercase().contains(&normalized_query));
    }
}

fn public_event_rows(screen: MailScreenId, state: &TuiSharedState) -> Vec<String> {
    public_event_rows_from_events(screen, state.replay_events())
}

fn public_event_rows_from_events(
    screen: MailScreenId,
    events: impl IntoIterator<Item = MailEvent>,
) -> Vec<String> {
    events
        .into_iter()
        .filter(|event| event_visible_on(screen, event))
        .map(|event| public_event_row(&event))
        .collect()
}

fn event_visible_on(screen: MailScreenId, event: &MailEvent) -> bool {
    match screen {
        MailScreenId::Messages | MailScreenId::Threads | MailScreenId::Attachments => matches!(
            event,
            MailEvent::MessageSent { .. } | MailEvent::MessageReceived { .. }
        ),
        MailScreenId::ToolMetrics | MailScreenId::Analytics => matches!(
            event,
            MailEvent::ToolCallStart { .. }
                | MailEvent::ToolCallEnd { .. }
                | MailEvent::HttpRequest { .. }
                | MailEvent::GitSegfaultRetry { .. }
        ),
        MailScreenId::Dashboard
        | MailScreenId::Agents
        | MailScreenId::Reservations
        | MailScreenId::SystemHealth
        | MailScreenId::Projects
        | MailScreenId::Contacts
        | MailScreenId::ArchiveBrowser
        | MailScreenId::Atc => false,
        MailScreenId::Search | MailScreenId::Timeline | MailScreenId::Explorer => true,
    }
}

fn public_event_row(event: &MailEvent) -> String {
    #[cfg(test)]
    PUBLIC_EVENT_FORMAT_COUNT.with(|count| count.set(count.get().saturating_add(1)));

    let prefix = format!("#{:<5} {:<18}", event.seq(), format!("{:?}", event.kind()));
    let detail = match event {
        MailEvent::MessageSent {
            from,
            to,
            subject,
            project,
            ..
        }
        | MailEvent::MessageReceived {
            from,
            to,
            subject,
            project,
            ..
        } => format!("{from} → {} · {subject} · {project}", to.join(",")),
        MailEvent::ReservationGranted {
            agent,
            paths,
            project,
            ..
        }
        | MailEvent::ReservationReleased {
            agent,
            paths,
            project,
            ..
        } => format!("{agent} · {} · {project}", paths.join(",")),
        MailEvent::AgentRegistered {
            name,
            program,
            model_name,
            project,
            ..
        } => format!("{name} · {program}/{model_name} · {project}"),
        MailEvent::ToolCallStart {
            tool_name,
            project,
            agent,
            ..
        }
        | MailEvent::ToolCallEnd {
            tool_name,
            project,
            agent,
            ..
        } => format!(
            "{tool_name} · {} · {}",
            agent.as_deref().unwrap_or("system"),
            project.as_deref().unwrap_or("global")
        ),
        MailEvent::HttpRequest {
            method,
            path,
            status,
            duration_ms,
            ..
        } => format!("{method} {path} · {status} · {duration_ms}ms"),
        MailEvent::HealthPulse { db_stats, .. } => format!(
            "projects:{} agents:{} messages:{}",
            db_stats.projects, db_stats.agents, db_stats.messages
        ),
        MailEvent::GitSegfaultRetry {
            name,
            repo_slug,
            attempt_n,
            ..
        } => format!("{name} · {repo_slug} · attempt {attempt_n}"),
        MailEvent::ServerStarted { endpoint, .. } => endpoint.clone(),
        MailEvent::ServerShutdown { .. } => "server stopped".to_string(),
    };
    format!("{prefix} {detail}")
}

#[cfg(test)]
thread_local! {
    static PUBLIC_EVENT_FORMAT_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_public_event_format_count() {
    PUBLIC_EVENT_FORMAT_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn public_event_format_count() -> usize {
    PUBLIC_EVENT_FORMAT_COUNT.with(Cell::get)
}

#[path = "../../mcp-agent-mail-server/src/tui_screens/dashboard.rs"]
pub mod dashboard;

#[cfg(test)]
mod tests {
    use super::{
        MailScreenId, PublicReplayScreen, public_event_format_count, public_rows,
        public_rows_generation, reset_public_event_format_count, search_input_hit_area,
    };
    use crate::demo_pack::{DemoOperation, curated_public_demo};
    use crate::state::TuiSharedState;
    use crate::tui_events::{EventSource, MailEvent};
    use ftui::layout::Rect;
    use ftui::{Event, MouseButton, MouseEvent, MouseEventKind};

    fn populated_state() -> TuiSharedState {
        let pack = curated_public_demo();
        let state = TuiSharedState::new();
        pack.apply_bootstrap(&state);
        for (index, action) in pack.actions.iter().enumerate() {
            if action.at_ms != 0 {
                break;
            }
            if matches!(&action.operation, DemoOperation::PublishEvent { .. }) {
                pack.apply_action(index, &state);
            }
        }
        state
    }

    fn public_http(path: impl Into<String>) -> MailEvent {
        MailEvent::HttpRequest {
            seq: 0,
            timestamp_micros: 1,
            source: EventSource::Http,
            redacted: true,
            method: "GET".to_string(),
            path: path.into(),
            status: 200,
            duration_ms: 1,
            client_ip: "synthetic-client".to_string(),
        }
    }

    fn cached_rows(screen: &PublicReplayScreen) -> Vec<String> {
        screen
            .row_cache
            .borrow()
            .as_ref()
            .expect("rows should be materialized")
            .rows
            .clone()
    }

    #[test]
    fn public_projections_are_screen_specific_and_authority_bounded() {
        let state = populated_state();
        let messages = public_rows(MailScreenId::Messages, &state);
        let threads = public_rows(MailScreenId::Threads, &state);
        let tools = public_rows(MailScreenId::ToolMetrics, &state);
        let attachments = public_rows(MailScreenId::Attachments, &state);
        let atc = public_rows(MailScreenId::Atc, &state);

        assert!(!messages.is_empty());
        assert!(!threads.is_empty());
        assert_ne!(threads, messages);
        assert!(threads.iter().all(|row| row.contains("messages:")));
        assert!(!tools.is_empty(), "the tool screen must not open blank");
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].contains("No attachment metadata"));
        assert!(atc.iter().all(|row| {
            let lower = row.to_ascii_lowercase();
            !["hold", "clear", "evidence", "conflict"]
                .iter()
                .any(|claim| lower.contains(claim))
        }));
    }

    #[test]
    fn clicking_below_the_last_visible_row_is_a_noop() {
        let state = populated_state();
        let row_count = public_rows(MailScreenId::Messages, &state).len();
        assert!(row_count >= 2);

        let mut screen = PublicReplayScreen::new();
        screen.list_area.set(Rect::new(10, 5, 80, 5));
        screen.scroll.set(row_count - 2);

        let blank_click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            12,
            9,
        ));
        assert!(!screen.update(&blank_click, MailScreenId::Messages, &state));
        assert_eq!(screen.selected_row(), 0);

        let last_row_click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            12,
            6,
        ));
        assert!(screen.update(&last_row_click, MailScreenId::Messages, &state));
        assert_eq!(screen.selected_row(), row_count - 1);
    }

    #[test]
    fn shrinking_the_rendered_page_keeps_selection_visible() {
        let screen = PublicReplayScreen::new();
        screen.selected.set(9);
        screen.scroll.set(0);

        screen.clamp_to_rows(20, 10);
        assert_eq!(screen.scroll.get(), 0);

        screen.clamp_to_rows(20, 3);
        assert_eq!(screen.selected_row(), 9);
        assert_eq!(screen.scroll.get(), 7);

        screen.selected.set(19);
        screen.scroll.set(19);
        screen.clamp_to_rows(20, 3);
        assert_eq!(screen.selected_row(), 19);
        assert_eq!(screen.scroll.get(), 17);
    }

    #[test]
    fn public_projection_generations_ignore_unrelated_replay_channels() {
        let state = populated_state();
        let before = state.data_generation();
        state.record_request(200, 12);
        let after_request = state.data_generation();

        assert_eq!(
            public_rows_generation(MailScreenId::Messages, before),
            public_rows_generation(MailScreenId::Messages, after_request),
            "request telemetry must not invalidate materialized message rows"
        );
        assert_ne!(
            public_rows_generation(MailScreenId::Analytics, before),
            public_rows_generation(MailScreenId::Analytics, after_request),
            "analytics includes request telemetry and must be invalidated"
        );
    }

    #[test]
    fn timeline_focus_selects_the_nearest_event_timestamp() {
        let state = populated_state();
        let events = state.tick_events_since_limited(0, 2_000);
        let target_index = 37;
        let target = events[target_index].timestamp_micros();
        let mut screen = PublicReplayScreen::new();
        screen.list_area.set(Rect::new(0, 0, 120, 12));

        assert!(screen.focus_timeline_at(target, &state));
        assert_eq!(screen.selected_row(), target_index);
    }

    #[test]
    fn public_event_projections_include_newest_rows_beyond_two_thousand_events() {
        let state = populated_state();
        let seed = state
            .replay_events()
            .into_iter()
            .next()
            .expect("populated replay should contain an event");
        for _ in 0..2_100 {
            assert!(state.push_event(seed.clone()));
        }

        let stats = state.event_ring_stats();
        let rows = public_rows(MailScreenId::Explorer, &state);
        let newest_seq = stats.next_seq.saturating_sub(1);
        assert_eq!(rows.len(), stats.len);
        assert!(
            rows.last()
                .is_some_and(|row| row.starts_with(&format!("#{newest_seq:<5}"))),
            "the newest retained event must remain visible after 2,000 rows"
        );
    }

    #[test]
    fn direct_event_projection_formats_only_the_new_tail() {
        let state = TuiSharedState::new_with_replay_event_capacity(256);
        for index in 0..40 {
            assert!(state.push_event(public_http(format!("/seed/{index}"))));
        }
        let screen = PublicReplayScreen::new();

        reset_public_event_format_count();
        screen.ensure_rows(MailScreenId::Explorer, &state);
        assert_eq!(cached_rows(&screen).len(), 40);
        assert_eq!(public_event_format_count(), 40);

        for index in 0..50 {
            assert!(state.push_event(public_http(format!("/tail/{index}"))));
            screen.ensure_rows(MailScreenId::Explorer, &state);
        }

        assert_eq!(cached_rows(&screen).len(), 90);
        assert_eq!(
            public_event_format_count(),
            90,
            "repeated refreshes must format each retained event once, not rebuild the cumulative history"
        );
    }

    #[test]
    fn direct_event_projection_rebuilds_after_reset_with_equal_counts() {
        let state = TuiSharedState::new_with_replay_event_capacity(8);
        assert!(state.push_event(public_http("/old/one")));
        assert!(state.push_event(public_http("/old/two")));
        let screen = PublicReplayScreen::new();
        screen.ensure_rows(MailScreenId::Explorer, &state);

        state.reset();
        assert!(state.push_event(public_http("/new/one")));
        assert!(state.push_event(public_http("/new/two")));
        reset_public_event_format_count();
        screen.ensure_rows(MailScreenId::Explorer, &state);

        let rows = cached_rows(&screen);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.contains("/new/")));
        assert!(rows.iter().all(|row| !row.contains("/old/")));
        assert_eq!(
            public_event_format_count(),
            2,
            "a reset epoch must force a complete replacement even when the event count matches"
        );
    }

    #[test]
    fn direct_event_projection_rebuilds_after_ring_eviction() {
        let state = TuiSharedState::new_with_replay_event_capacity(4);
        for path in ["/oldest", "/second", "/third", "/fourth"] {
            assert!(state.push_event(public_http(path)));
        }
        let screen = PublicReplayScreen::new();
        screen.ensure_rows(MailScreenId::Explorer, &state);

        reset_public_event_format_count();
        assert!(state.push_event(public_http("/newest")));
        screen.ensure_rows(MailScreenId::Explorer, &state);

        let rows = cached_rows(&screen);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| !row.contains("/oldest")));
        assert!(rows.iter().any(|row| row.contains("/newest")));
        assert_eq!(
            public_event_format_count(),
            4,
            "an eviction changes retained history and must force a full rebuild"
        );
    }

    #[test]
    fn search_filter_and_screen_changes_preserve_incremental_correctness() {
        let state = TuiSharedState::new_with_replay_event_capacity(16);
        assert!(state.push_event(public_http("/keep/one")));
        assert!(state.push_event(public_http("/drop/one")));
        let mut screen = PublicReplayScreen::new();
        screen.begin_search("keep");
        screen.ensure_rows(MailScreenId::Search, &state);
        assert_eq!(cached_rows(&screen).len(), 1);

        reset_public_event_format_count();
        assert!(state.push_event(public_http("/keep/two")));
        screen.ensure_rows(MailScreenId::Search, &state);
        assert!(state.push_event(public_http("/drop/two")));
        screen.ensure_rows(MailScreenId::Search, &state);
        let keep_rows = cached_rows(&screen);
        assert_eq!(keep_rows.len(), 2);
        assert!(keep_rows.iter().all(|row| row.contains("/keep/")));
        assert_eq!(public_event_format_count(), 2);

        screen.begin_search("drop");
        reset_public_event_format_count();
        screen.ensure_rows(MailScreenId::Search, &state);
        let drop_rows = cached_rows(&screen);
        assert_eq!(drop_rows.len(), 2);
        assert!(drop_rows.iter().all(|row| row.contains("/drop/")));
        assert_eq!(
            public_event_format_count(),
            4,
            "changing the query must refilter a complete snapshot"
        );

        reset_public_event_format_count();
        screen.ensure_rows(MailScreenId::ToolMetrics, &state);
        assert_eq!(cached_rows(&screen).len(), 4);
        assert_eq!(
            public_event_format_count(),
            4,
            "changing screens must not append rows derived for another projection"
        );
    }

    #[test]
    fn clicking_the_search_input_reactivates_editing_without_erasing_query() {
        let state = populated_state();
        let mut screen = PublicReplayScreen::new();
        screen.begin_search("release");
        screen.search_active = false;
        screen.search_input_area.set(Rect::new(5, 3, 80, 1));

        let click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            20,
            3,
        ));
        assert!(screen.update(&click, MailScreenId::Search, &state));
        assert!(screen.consumes_text_input(MailScreenId::Search));
        assert_eq!(screen.search_query(), "release");
    }

    #[test]
    fn clipped_search_prompt_has_no_invisible_mouse_hitbox() {
        for height in [0, 1, 2] {
            assert_eq!(
                search_input_hit_area(MailScreenId::Search, Rect::new(5, 7, 80, height)),
                Rect::new(0, 0, 0, 0)
            );
        }
        assert_eq!(
            search_input_hit_area(MailScreenId::Search, Rect::new(5, 7, 80, 3)),
            Rect::new(5, 9, 80, 1)
        );
        assert_eq!(
            search_input_hit_area(MailScreenId::Messages, Rect::new(5, 7, 80, 3)),
            Rect::new(0, 0, 0, 0)
        );
    }
}
