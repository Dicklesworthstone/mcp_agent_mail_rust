//! FrankenTUI model that renders the production Agent Mail dashboard.

use ftui::{Event, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind};
use ftui_runtime::program::{Cmd, Model};

use crate::demo_pack::{DemoPack, DemoPackError, curated_public_demo};
use crate::state::TuiSharedState;
use crate::tui_hit_regions::{MouseAction, MouseDispatcher};
use crate::tui_keymap::HelpSection;
use crate::tui_persist::AccessibilitySettings;
use crate::tui_screens::dashboard::DashboardScreen;
use crate::tui_screens::{
    DeepLinkTarget, HelpEntry, MailScreen, MailScreenId, MailScreenMsg, PublicReplayScreen,
    browser_projection_description, screen_from_jump_key, screen_meta,
};

#[derive(Debug, Clone)]
pub struct DashboardMessage(pub Event);

impl From<Event> for DashboardMessage {
    fn from(event: Event) -> Self {
        Self(event)
    }
}

/// Result of advancing the deterministic replay independently of rendering.
///
/// The browser runner uses `advanced_ms` to derive production-parity logical
/// ticks without tying replay time to the host's animation-frame cadence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReplayAdvance {
    pub(crate) advanced_ms: u64,
    pub(crate) visible_changed: bool,
    pub(crate) replay_reset: bool,
}

/// State owned by the browser runner. The dashboard and replay adapter are
/// intentionally in-memory and expose no mailbox mutation capability.
pub struct DashboardModel {
    screen: DashboardScreen,
    public_screen: PublicReplayScreen,
    state: TuiSharedState,
    pack: DemoPack,
    active_screen: MailScreenId,
    mouse_dispatcher: MouseDispatcher,
    accessibility: AccessibilitySettings,
    help_visible: bool,
    help_scroll: u16,
    last_terminal_area: std::cell::Cell<ftui::layout::Rect>,
    interaction_revision: u64,
    next_action: usize,
    elapsed_ms: u64,
    tick_count: u64,
    paused: bool,
    reduced_motion: bool,
    last_deep_link: Option<String>,
    prepared: bool,
    #[cfg(test)]
    replay_reset_count: u64,
}

impl Default for DashboardModel {
    fn default() -> Self {
        Self::new(curated_public_demo())
    }
}

impl DashboardModel {
    #[must_use]
    pub fn new(pack: DemoPack) -> Self {
        let mut model = Self {
            screen: DashboardScreen::new(),
            public_screen: PublicReplayScreen::new(),
            state: TuiSharedState::new(),
            pack,
            active_screen: MailScreenId::Dashboard,
            mouse_dispatcher: MouseDispatcher::new(),
            accessibility: AccessibilitySettings::default(),
            help_visible: false,
            help_scroll: 0,
            last_terminal_area: std::cell::Cell::new(ftui::layout::Rect::new(0, 0, 0, 0)),
            interaction_revision: 0,
            next_action: 0,
            elapsed_ms: 0,
            tick_count: 0,
            paused: false,
            reduced_motion: false,
            last_deep_link: None,
            prepared: false,
            #[cfg(test)]
            replay_reset_count: 0,
        };
        model.reset();
        model
    }

    /// Construct a lightweight host runner before its verified external pack
    /// is available. Unlike `Default`, this performs no synthetic data
    /// generation, validation, bootstrap cloning, or initial screen ticks.
    #[must_use]
    pub fn unloaded() -> Self {
        Self {
            screen: DashboardScreen::new(),
            public_screen: PublicReplayScreen::new(),
            state: TuiSharedState::new(),
            pack: DemoPack::unloaded_runner_placeholder(),
            active_screen: MailScreenId::Dashboard,
            mouse_dispatcher: MouseDispatcher::new(),
            accessibility: AccessibilitySettings::default(),
            help_visible: false,
            help_scroll: 0,
            last_terminal_area: std::cell::Cell::new(ftui::layout::Rect::new(0, 0, 0, 0)),
            interaction_revision: 0,
            next_action: 0,
            elapsed_ms: 0,
            tick_count: 0,
            paused: false,
            reduced_motion: false,
            last_deep_link: None,
            prepared: false,
            #[cfg(test)]
            replay_reset_count: 0,
        }
    }

    fn fresh_screen(&self) -> DashboardScreen {
        let mut screen = DashboardScreen::new();
        screen.set_motion_preferences(self.reduced_motion, !self.reduced_motion);
        screen
    }

    pub(crate) fn sync_replay_clock(&self) {
        let elapsed_micros = i64::try_from(self.elapsed_ms)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000);
        let now_micros = self
            .pack
            .bootstrap
            .db_stats
            .timestamp_micros
            .saturating_add(elapsed_micros);
        crate::tui_screens::dashboard::set_browser_replay_now_micros(now_micros);
    }

    pub fn load_pack(&mut self, pack: DemoPack) -> Result<(), DemoPackError> {
        pack.validate()?;
        self.pack = pack;
        self.reset();
        Ok(())
    }

    pub fn load_pack_json(&mut self, json: &str) -> Result<(), DemoPackError> {
        // `from_json` is the single validation boundary. Do not recursively
        // validate the same large pack again through `load_pack`.
        self.pack = DemoPack::from_json(json)?;
        self.reset();
        Ok(())
    }

    pub fn reset(&mut self) {
        self.reset_replay(false);
    }

    fn reset_replay(&mut self, preserve_interaction: bool) {
        #[cfg(test)]
        {
            self.replay_reset_count = self.replay_reset_count.saturating_add(1);
        }
        let preserved_screen = self.active_screen;
        let preserved_help = self.help_visible;
        let preserved_help_scroll = self.help_scroll;
        let preserved_deep_link = self.last_deep_link.clone();
        let dashboard_interaction = self.screen.interaction_snapshot();
        self.screen = self.fresh_screen();
        if preserve_interaction {
            self.screen.restore_interaction(&dashboard_interaction);
        } else {
            self.public_screen.reset();
        }
        self.pack.apply_bootstrap(&self.state);
        if preserve_interaction {
            self.active_screen = preserved_screen;
            self.help_visible = preserved_help;
            self.help_scroll = preserved_help_scroll;
            self.last_deep_link = preserved_deep_link;
        } else {
            self.active_screen = MailScreenId::Dashboard;
            self.help_visible = false;
            self.help_scroll = 0;
            self.last_deep_link = None;
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
        self.next_action = 0;
        self.elapsed_ms = 0;
        self.apply_due_actions();
        // Prime two production stat-refresh boundaries so the first browser
        // frame has real percentile and throughput history instead of empty
        // chart shells. DashboardScreen refreshes those histories every ten
        // logical ticks in both native and browser builds.
        self.tick_count = 10;
        self.state.set_elapsed_ms(0);
        self.sync_replay_clock();
        self.screen.tick(0, &self.state);
        self.screen.tick(self.tick_count, &self.state);
        // Public replay rows are materialized lazily by view/input. Deferring
        // their normalization avoids rebuilding a 10,000-row projection while
        // catch-up replay is still applying actions that will be discarded or
        // superseded before the next browser frame.
        self.prepared = true;
    }

    /// Discard prior loop cycles and restore the deterministic beginning of
    /// the surviving cycle. The runner then advances that one cycle at native
    /// logical-tick boundaries so history samples retain their exact timing.
    pub(crate) fn restart_looping_replay_cycle(&mut self) {
        debug_assert!(self.pack.loop_replay);
        self.reset_replay(true);
    }

    /// Advance the deterministic replay clock. Returns whether visible state
    /// may have changed and therefore needs a host-driven render.
    pub fn advance_replay_ms(&mut self, dt_ms: u64) -> bool {
        self.advance_replay(dt_ms).visible_changed
    }

    /// Advance replay state and report the exact amount of logical time that
    /// was consumed. Loop reconstruction is constant in the number of cycles:
    /// prior cycles cannot affect the final replay snapshot, so only the final
    /// cycle's bootstrap and due actions need to be applied.
    pub(crate) fn advance_replay(&mut self, dt_ms: u64) -> ReplayAdvance {
        if self.paused || dt_ms == 0 {
            return ReplayAdvance::default();
        }

        let previous_clock_second = self.elapsed_ms / 1_000;
        let actions_applied;
        let replay_reset;
        let duration_ms = self.pack.duration_ms;
        if duration_ms == 0 {
            return ReplayAdvance::default();
        }
        let total_ms = u128::from(self.elapsed_ms) + u128::from(dt_ms);
        let advanced_ms;

        if total_ms <= u128::from(duration_ms) {
            self.elapsed_ms = u64::try_from(total_ms).unwrap_or(duration_ms);
            actions_applied = self.apply_due_actions();
            replay_reset = false;
            advanced_ms = dt_ms;
        } else if !self.pack.loop_replay {
            advanced_ms = duration_ms.saturating_sub(self.elapsed_ms);
            self.elapsed_ms = duration_ms;
            actions_applied = self.apply_due_actions();
            replay_reset = false;
        } else {
            let remainder = (total_ms - u128::from(duration_ms)) % u128::from(duration_ms);
            let final_elapsed_ms = if remainder == 0 {
                duration_ms
            } else {
                u64::try_from(remainder).unwrap_or(duration_ms)
            };

            self.reset_replay(true);
            self.elapsed_ms = final_elapsed_ms;
            actions_applied = self.apply_due_actions();
            replay_reset = true;
            advanced_ms = dt_ms;
        }

        self.state.set_elapsed_ms(self.elapsed_ms);
        self.sync_replay_clock();
        ReplayAdvance {
            advanced_ms,
            visible_changed: actions_applied
                || replay_reset
                || self.elapsed_ms / 1_000 != previous_clock_second,
            replay_reset,
        }
    }

    fn apply_due_actions(&mut self) -> bool {
        let starting_action = self.next_action;
        while self
            .pack
            .actions
            .get(self.next_action)
            .is_some_and(|action| action.at_ms <= self.elapsed_ms)
        {
            self.pack.apply_action(self.next_action, &self.state);
            self.next_action = self.next_action.saturating_add(1);
        }
        self.next_action != starting_action
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.reduced_motion = reduced_motion;
        self.accessibility.reduced_motion = reduced_motion;
        self.screen
            .set_motion_preferences(reduced_motion, !reduced_motion);
    }

    #[must_use]
    pub const fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    #[must_use]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    #[must_use]
    pub const fn reduced_motion(&self) -> bool {
        self.reduced_motion
    }

    #[must_use]
    pub const fn next_action(&self) -> usize {
        self.next_action
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn tick_count(&self) -> u64 {
        self.tick_count
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn replay_reset_count(&self) -> u64 {
        self.replay_reset_count
    }

    /// Apply one production-cadence logical tick without forcing an
    /// intermediate browser repaint.
    pub(crate) fn logical_tick(&mut self) {
        self.tick_count = self.tick_count.saturating_add(1);
        self.screen.tick(self.tick_count, &self.state);
    }

    #[must_use]
    pub const fn pack(&self) -> &DemoPack {
        &self.pack
    }

    #[must_use]
    pub fn last_deep_link(&self) -> Option<&str> {
        self.last_deep_link.as_deref()
    }

    #[must_use]
    pub const fn active_screen(&self) -> MailScreenId {
        self.active_screen
    }

    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.help_visible
    }

    #[must_use]
    pub const fn interaction_revision(&self) -> u64 {
        self.interaction_revision
    }

    #[must_use]
    pub const fn public_selected_row(&self) -> usize {
        self.public_screen.selected_row()
    }

    #[must_use]
    pub fn dashboard_filter_slug(&self) -> &'static str {
        self.screen.active_filter_slug()
    }

    #[must_use]
    pub fn state(&self) -> &TuiSharedState {
        &self.state
    }

    /// Open the browser Search screen and account for the entire shell
    /// transition as one semantic interaction, including same-screen query,
    /// editor, or selection resets.
    fn open_search(&mut self, query: &str) {
        let before = (
            self.active_screen,
            self.public_screen.search_query().to_string(),
            self.public_screen.consumes_text_input(MailScreenId::Search),
            self.public_screen.selected_row(),
            self.last_deep_link.clone(),
        );

        if self.active_screen != MailScreenId::Search {
            self.public_screen
                .switch_screen(self.active_screen, MailScreenId::Search);
            self.active_screen = MailScreenId::Search;
        }
        self.public_screen.begin_search(query);
        self.last_deep_link = Some(format!("search:{query}"));

        let after = (
            self.active_screen,
            self.public_screen.search_query().to_string(),
            self.public_screen.consumes_text_input(MailScreenId::Search),
            self.public_screen.selected_row(),
            self.last_deep_link.clone(),
        );
        if after != before {
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
    }

    fn capture_screen_command(&mut self, command: Cmd<MailScreenMsg>) {
        match command {
            Cmd::Msg(MailScreenMsg::DeepLink(target)) => {
                let label = match target {
                    DeepLinkTarget::TimelineAtTime(timestamp) => {
                        self.activate_screen(MailScreenId::Timeline);
                        let _ = self.public_screen.focus_timeline_at(timestamp, &self.state);
                        format!("timeline:{timestamp}")
                    }
                    DeepLinkTarget::SearchFocused(query) => {
                        self.open_search(&query);
                        return;
                    }
                };
                self.last_deep_link = Some(label);
            }
            Cmd::Batch(commands) | Cmd::Sequence(commands) => {
                for command in commands {
                    self.capture_screen_command(command);
                }
            }
            _ => {}
        }
    }

    fn activate_screen(&mut self, screen: MailScreenId) {
        if self.active_screen != screen {
            self.public_screen
                .switch_screen(self.active_screen, screen);
            self.active_screen = screen;
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
    }

    fn scroll_help_down(&mut self) {
        let previous = self.help_scroll;
        let max_scroll = browser_help_max_scroll(self.active_screen, self.last_terminal_area.get());
        self.help_scroll = self.help_scroll.saturating_add(1).min(max_scroll);
        if self.help_scroll != previous {
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
    }

    fn handle_shell_input(&mut self, event: &Event) -> bool {
        if self.help_visible {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Escape | KeyCode::F(1) | KeyCode::Char('?') => {
                        self.help_visible = false;
                        self.help_scroll = 0;
                        self.interaction_revision = self.interaction_revision.saturating_add(1);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.scroll_help_down();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let previous = self.help_scroll;
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                        if self.help_scroll != previous {
                            self.interaction_revision =
                                self.interaction_revision.saturating_add(1);
                        }
                    }
                    _ => {
                        self.help_visible = false;
                        self.help_scroll = 0;
                        self.interaction_revision = self.interaction_revision.saturating_add(1);
                    }
                },
                Event::Mouse(mouse) => {
                    let overlay = crate::tui_chrome::help_overlay_rect(self.last_terminal_area.get());
                    let inside = crate::tui_hit_regions::point_in_rect(overlay, mouse.x, mouse.y);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) if !inside => {
                            self.help_visible = false;
                            self.help_scroll = 0;
                            self.interaction_revision =
                                self.interaction_revision.saturating_add(1);
                        }
                        MouseEventKind::ScrollDown if inside => {
                            self.scroll_help_down();
                        }
                        MouseEventKind::ScrollUp if inside => {
                            let previous = self.help_scroll;
                            self.help_scroll = self.help_scroll.saturating_sub(1);
                            if self.help_scroll != previous {
                                self.interaction_revision =
                                    self.interaction_revision.saturating_add(1);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            return true;
        }

        let text_mode = if self.active_screen == MailScreenId::Dashboard {
            self.screen.consumes_text_input()
        } else {
            self.public_screen.consumes_text_input(self.active_screen)
        };

        if let Event::Mouse(mouse) = event {
            match self.mouse_dispatcher.dispatch(mouse) {
                MouseAction::SwitchScreen(screen) => {
                    self.activate_screen(screen);
                    return true;
                }
                MouseAction::ToggleHelp => {
                    self.help_visible = true;
                    self.help_scroll = 0;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                MouseAction::OpenPalette => {
                    self.open_search("");
                    return true;
                }
                MouseAction::Forward => {}
            }
        }

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            let is_ctrl_p = key.modifiers.contains(Modifiers::CTRL)
                && matches!(key.code, KeyCode::Char('p' | 'P'));
            if (is_ctrl_p || matches!(key.code, KeyCode::Char(':'))) && !text_mode {
                if self.active_screen == MailScreenId::Search
                    && self.public_screen.consumes_text_input(MailScreenId::Search)
                {
                    return true;
                }
                self.open_search("");
                return true;
            }

            match key.code {
                KeyCode::Tab => {
                    self.activate_screen(self.active_screen.next());
                    return true;
                }
                KeyCode::BackTab => {
                    self.activate_screen(self.active_screen.prev());
                    return true;
                }
                KeyCode::F(1) => {
                    self.help_visible = true;
                    self.help_scroll = 0;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                KeyCode::Char('?') if !text_mode => {
                    self.help_visible = true;
                    self.help_scroll = 0;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                KeyCode::Char('/') if !text_mode => {
                    if self.active_screen == MailScreenId::Search {
                        if self.public_screen.reactivate_search() {
                            self.interaction_revision = self.interaction_revision.saturating_add(1);
                        }
                    } else {
                        self.open_search("");
                    }
                    return true;
                }
                KeyCode::Char(character)
                    if !text_mode
                        && !key
                            .modifiers
                            .intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER) =>
                {
                    if let Some(screen) = screen_from_jump_key(character) {
                        self.activate_screen(screen);
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn screen_bindings(&self) -> Vec<HelpEntry> {
        if self.active_screen == MailScreenId::Dashboard {
            self.screen.keybindings()
        } else {
            vec![
                HelpEntry {
                    key: "↑/↓ j/k",
                    action: "Select record",
                },
                HelpEntry {
                    key: "Click",
                    action: "Select record or switch tab",
                },
            ]
        }
    }
}

impl Model for DashboardModel {
    type Message = DashboardMessage;

    fn init(&mut self) -> Cmd<Self::Message> {
        // `load_pack_json` prepares the complete opening frame before the host
        // calls init. Preserve it rather than cloning/applying the pack twice.
        if !self.prepared {
            self.reset();
        }
        Cmd::none()
    }

    fn update(&mut self, message: Self::Message) -> Cmd<Self::Message> {
        if matches!(message.0, Event::Tick) {
            self.logical_tick();
        } else if self.handle_shell_input(&message.0) {
            return Cmd::none();
        } else if self.active_screen == MailScreenId::Dashboard {
            let interaction_before = self.screen.interaction_snapshot();
            let command = self.screen.update(&message.0, &self.state);
            if self.screen.interaction_snapshot() != interaction_before {
                self.interaction_revision = self.interaction_revision.saturating_add(1);
            }
            self.capture_screen_command(command);
        } else if self
            .public_screen
            .update(&message.0, self.active_screen, &self.state)
        {
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
        Cmd::none()
    }

    fn view(&self, frame: &mut ftui::Frame<'_>) {
        // Dashboard time helpers use a thread-local compatibility clock. A
        // browser page can host multiple runners, so install this instance's
        // clock immediately before every render.
        self.sync_replay_clock();
        let area = ftui::layout::Rect::new(0, 0, frame.width(), frame.height());
        self.last_terminal_area.set(area);
        let chrome = crate::tui_chrome::chrome_layout(area);
        crate::tui_chrome::render_tab_bar(
            self.active_screen,
            !self.reduced_motion,
            frame,
            chrome.tab_bar,
        );
        crate::tui_chrome::record_tab_hit_slots(
            chrome.tab_bar,
            self.active_screen,
            &self.mouse_dispatcher,
        );
        self.mouse_dispatcher
            .update_chrome_areas(chrome.tab_bar, chrome.status_line);

        if self.active_screen == MailScreenId::Dashboard {
            self.screen.view(frame, chrome.content, &self.state);
        } else {
            self.public_screen
                .view(self.active_screen, frame, chrome.content, &self.state);
        }

        let bindings = self.screen_bindings();
        crate::tui_chrome::render_status_line_with_hits(
            &self.state,
            self.active_screen,
            "REPLAY",
            false,
            self.help_visible,
            &self.accessibility,
            &bindings,
            false,
            &self.mouse_dispatcher,
            frame,
            chrome.status_line,
        );

        if self.help_visible {
            render_browser_help(
                self.active_screen,
                self.help_scroll,
                !self.reduced_motion,
                frame,
                area,
            );
        }
    }
}

fn browser_help_sections(screen: MailScreenId) -> Vec<HelpSection> {
    let current_screen_entries = if screen == MailScreenId::Dashboard {
        vec![
            (
                "A / M / O / R".to_string(),
                "Dashboard All / Messages / Tools / Reservations filters".to_string(),
            ),
            (
                "j/k or Up/Down".to_string(),
                "Scroll Dashboard events".to_string(),
            ),
            (
                "Enter".to_string(),
                "Open the focused Timeline or Search context".to_string(),
            ),
        ]
    } else {
        vec![
            (
                "j/k or Up/Down".to_string(),
                "Select a public replay row".to_string(),
            ),
            (
                "PageUp/PageDown".to_string(),
                "Move by one visible page".to_string(),
            ),
            (
                "Click".to_string(),
                "Select a public replay row".to_string(),
            ),
        ]
    };

    vec![
        HelpSection {
            title: format!("{} · browser replay", screen_meta(screen).title),
            description: Some(browser_projection_description(screen).to_string()),
            body_markdown: Some(
                "Production DashboardScreen + shared chrome.\nRead-only browser replay; no mailbox mutations.\nNative command palette/actions are not included.\nCount-only SQLite totals seed synthetic details."
                    .to_string(),
            ),
            entries: current_screen_entries,
        },
        HelpSection {
            title: "Mouse".to_string(),
            description: None,
            body_markdown: None,
            entries: vec![
                ("Top tabs".to_string(), "Switch screens".to_string()),
                ("< or >".to_string(), "Reveal hidden top tabs".to_string()),
                (
                    "Dashboard controls".to_string(),
                    "Choose filters or edit Live Filter".to_string(),
                ),
                (
                    "Wheel".to_string(),
                    "Scroll the panel under the pointer".to_string(),
                ),
            ],
        },
        HelpSection {
            title: "Global keyboard".to_string(),
            description: None,
            body_markdown: None,
            entries: vec![
                (
                    "Tab / Shift+Tab".to_string(),
                    "Next / previous screen".to_string(),
                ),
                (
                    "1-9, 0, ! through ^".to_string(),
                    "Direct screen jump".to_string(),
                ),
                ("/".to_string(), "Open Search".to_string()),
                (
                    "Ctrl+P or :".to_string(),
                    "Open the browser Search adapter".to_string(),
                ),
                ("F1 or ?".to_string(), "Toggle this help".to_string()),
                (
                    "j/k while help is open".to_string(),
                    "Scroll this overlay".to_string(),
                ),
            ],
        },
    ]
}

fn browser_help_max_scroll(screen: MailScreenId, area: ftui::layout::Rect) -> u16 {
    let total_lines = browser_help_sections(screen)
        .iter()
        .map(|section| section.line_count().saturating_add(1))
        .sum::<usize>()
        .saturating_sub(1);
    let inner_height = crate::tui_chrome::help_overlay_rect(area)
        .height
        .saturating_sub(2);
    u16::try_from(total_lines.saturating_sub(usize::from(inner_height))).unwrap_or(u16::MAX)
}

fn render_browser_help(
    screen: MailScreenId,
    scroll: u16,
    effects_enabled: bool,
    frame: &mut ftui::Frame<'_>,
    area: ftui::layout::Rect,
) {
    let sections = browser_help_sections(screen);
    crate::tui_chrome::render_help_overlay_sections(
        &sections,
        scroll,
        effects_enabled,
        frame,
        area,
    );
}

#[cfg(test)]
mod tests {
    use ftui::{
        Event, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
        layout::Rect,
    };
    use ftui_runtime::program::Model;

    use super::{DashboardMessage, DashboardModel};
    use crate::demo_pack::{DemoAction, DemoOperation, curated_public_demo};
    use crate::tui_screens::{MailScreen, MailScreenId};

    #[test]
    fn replay_reset_is_deterministic() {
        let mut model = DashboardModel::new(curated_public_demo());
        let initial_action = model.next_action();
        let startup_actions = model
            .pack()
            .actions
            .iter()
            .take_while(|action| action.at_ms == 0)
            .count();
        assert_eq!(initial_action, startup_actions);
        assert_eq!(model.state().event_ring_stats().len, startup_actions);
        assert!(model.advance_replay_ms(10_000));
        assert_eq!(model.next_action(), initial_action + 7);
        model.reset();
        assert_eq!(model.elapsed_ms(), 0);
        assert_eq!(model.next_action(), initial_action);
        assert_eq!(
            model.state().db_stats_snapshot(),
            Some(model.pack().bootstrap.db_stats.clone())
        );
    }

    #[test]
    fn paused_replay_does_not_advance() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.set_paused(true);
        assert!(!model.advance_replay_ms(5_000));
        assert_eq!(model.elapsed_ms(), 0);
    }

    #[test]
    fn replay_only_marks_visible_clock_boundaries_dirty_between_actions() {
        let mut model = DashboardModel::new(curated_public_demo());

        assert!(!model.advance_replay_ms(100));
        assert!(!model.advance_replay_ms(899));
        assert!(model.advance_replay_ms(1));
        assert_eq!(model.elapsed_ms(), 1_000);
    }

    #[test]
    fn automatic_replay_wrap_preserves_active_screen() {
        let mut model = DashboardModel::new(curated_public_demo());
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Tab))));
        assert_eq!(model.active_screen(), MailScreenId::Messages);

        assert!(model.advance_replay_ms(model.pack().duration_ms + 100));

        assert_eq!(model.active_screen(), MailScreenId::Messages);
        assert_eq!(model.elapsed_ms(), 100);
    }

    #[test]
    fn automatic_replay_wrap_preserves_complete_dashboard_interaction() {
        let mut model = DashboardModel::new(curated_public_demo());
        for key in ['v', 'p', 'l', 'k'] {
            let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
                key,
            )))));
        }
        model.screen.set_query_interaction("reservation", true);
        let expected = model.screen.interaction_snapshot();

        assert!(model.advance_replay_ms(model.pack().duration_ms + 100));

        assert_eq!(model.screen.interaction_snapshot(), expected);
    }

    #[test]
    fn dashboard_interactions_advance_the_semantic_revision() {
        let mut model = DashboardModel::new(curated_public_demo());
        let before = model.interaction_revision();

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            'v',
        )))));

        assert!(model.interaction_revision() > before);
    }

    #[test]
    fn replay_clock_uses_absolute_pack_time_instead_of_unix_epoch_zero() {
        let mut model = DashboardModel::new(curated_public_demo());
        let base = model.pack().bootstrap.db_stats.timestamp_micros;
        assert_eq!(
            crate::tui_screens::dashboard::browser_replay_now_micros(),
            Some(base)
        );

        assert!(model.advance_replay_ms(2_500));
        assert_eq!(
            crate::tui_screens::dashboard::browser_replay_now_micros(),
            Some(base + 2_500_000)
        );
    }

    #[test]
    fn huge_looping_advance_reconstructs_only_the_final_cycle() {
        let pack = curated_public_demo();
        let duration_ms = pack.duration_ms;
        let mut expected = DashboardModel::new(pack.clone());
        let mut actual = DashboardModel::new(pack);

        assert!(expected.advance_replay_ms(6_100));
        assert!(
            actual.advance_replay_ms(duration_ms.saturating_mul(1_000_000).saturating_add(6_100),)
        );

        assert_eq!(actual.elapsed_ms(), expected.elapsed_ms());
        assert_eq!(actual.next_action(), expected.next_action());
        assert_eq!(
            actual.state().db_stats_snapshot(),
            expected.state().db_stats_snapshot()
        );
        assert_eq!(
            actual.state().request_counters(),
            expected.state().request_counters()
        );
        assert_eq!(
            actual.state().event_ring_stats(),
            expected.state().event_ring_stats()
        );
    }

    #[test]
    fn huge_exact_looping_advance_preserves_endpoint_semantics() {
        let pack = curated_public_demo();
        let duration_ms = pack.duration_ms;
        let mut expected = DashboardModel::new(pack.clone());
        let mut actual = DashboardModel::new(pack);

        assert!(expected.advance_replay_ms(duration_ms));
        assert!(actual.advance_replay_ms(duration_ms.saturating_mul(1_000_000)));

        assert_eq!(actual.elapsed_ms(), duration_ms);
        assert_eq!(actual.next_action(), expected.next_action());
        assert_eq!(
            actual.state().db_stats_snapshot(),
            expected.state().db_stats_snapshot()
        );
    }

    #[test]
    fn dashboard_text_mode_receives_digits_instead_of_switching_tabs() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.screen.set_query_interaction("", true);

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '2',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert_eq!(model.screen.interaction_snapshot().query(), "2");
    }

    #[test]
    fn browser_help_scrolls_and_only_outside_clicks_dismiss_it() {
        let mut model = DashboardModel::new(curated_public_demo());
        let area = Rect::new(0, 0, 70, 18);
        model.last_terminal_area.set(area);
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::F(1)))));
        assert!(model.help_visible());
        assert_eq!(model.help_scroll, 0);

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Down))));
        assert_eq!(model.help_scroll, 1);
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Up))));
        assert_eq!(model.help_scroll, 0);

        for _ in 0..100 {
            let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Down))));
        }
        assert_eq!(
            model.help_scroll,
            super::browser_help_max_scroll(MailScreenId::Dashboard, area)
        );
        let saturated_revision = model.interaction_revision();
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Down))));
        assert_eq!(model.interaction_revision(), saturated_revision);
        while model.help_scroll > 0 {
            let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Up))));
        }

        let overlay = crate::tui_chrome::help_overlay_rect(area);
        let inside_x = overlay.x.saturating_add(1);
        let inside_y = overlay.y.saturating_add(1);
        let _ = model.update(DashboardMessage(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            x: inside_x,
            y: inside_y,
            modifiers: Modifiers::empty(),
        })));
        assert_eq!(model.help_scroll, 1);

        let _ = model.update(DashboardMessage(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: inside_x,
            y: inside_y,
            modifiers: Modifiers::empty(),
        })));
        assert!(model.help_visible());
        assert_eq!(model.help_scroll, 1);

        let _ = model.update(DashboardMessage(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: 0,
            y: 0,
            modifiers: Modifiers::empty(),
        })));
        assert!(!model.help_visible());
        assert_eq!(model.help_scroll, 0);
    }

    #[test]
    fn dashboard_numeric_shortcuts_match_native_global_screen_jumps() {
        for (key, expected_screen) in [
            ('1', MailScreenId::Dashboard),
            ('2', MailScreenId::Messages),
            ('3', MailScreenId::Threads),
            ('4', MailScreenId::Agents),
        ] {
            let mut model = DashboardModel::new(curated_public_demo());
            let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
                key,
            )))));

            assert_eq!(model.active_screen(), expected_screen, "jump key {key}");
            if key == '1' {
                assert_eq!(
                    model.dashboard_filter_slug(),
                    "all",
                    "the native global jump must not activate a Dashboard quick filter"
                );
            }
        }
    }

    #[test]
    fn dashboard_slash_matches_native_global_search() {
        let mut model = DashboardModel::new(curated_public_demo());
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '/',
        )))));
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            'r',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.public_screen.search_query(), "r");
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
    }

    #[test]
    fn slash_on_a_public_screen_opens_an_editable_public_search() {
        let mut model = DashboardModel::new(curated_public_demo());
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Tab))));
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '/',
        )))));
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            'r',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.public_screen.search_query(), "r");
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
    }

    #[test]
    fn slash_reactivates_inactive_search_without_clearing_query_or_selection() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.open_search("release");
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Enter))));
        assert!(
            !model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::End))));
        let selection_before = model.public_selected_row();
        let revision_before = model.interaction_revision();
        assert!(
            selection_before > 0,
            "release query should have multiple rows"
        );

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '/',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.public_screen.search_query(), "release");
        assert_eq!(model.public_selected_row(), selection_before);
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
        assert!(model.interaction_revision() > revision_before);
    }

    #[test]
    fn direct_screen_jumps_remain_available_outside_dashboard() {
        let mut model = DashboardModel::new(curated_public_demo());
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Tab))));
        assert_eq!(model.active_screen(), MailScreenId::Messages);

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '4',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Agents);
    }

    #[test]
    fn timed_roster_shrink_clamps_public_selection_on_next_observation() {
        let mut pack = curated_public_demo();
        let mut smaller_snapshot = pack.bootstrap.db_stats.clone();
        smaller_snapshot.agents = 1;
        smaller_snapshot.agents_list.truncate(1);
        pack.actions.retain(|action| action.at_ms == 0);
        pack.actions.push(DemoAction {
            at_ms: 1,
            operation: DemoOperation::SetDbStats {
                snapshot: smaller_snapshot,
            },
        });
        pack.duration_ms = 100;
        pack.loop_replay = false;
        pack.finalize_digest();

        let mut model = DashboardModel::new(pack);
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Tab))));
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '4',
        )))));
        assert_eq!(model.active_screen(), MailScreenId::Agents);
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::End))));
        assert!(model.public_selected_row() > 0);

        assert!(model.advance_replay_ms(1));
        assert!(
            model.public_selected_row() > 0,
            "replay ingestion must defer expensive row projection until observation"
        );
        // Replay ingestion deliberately defers row materialization. The next
        // view or input observation rebuilds once and clamps before use.
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Down))));
        assert_eq!(model.public_selected_row(), 0);
        assert_eq!(
            model.state().db_stats_snapshot().unwrap().agents_list.len(),
            1
        );
    }

    #[test]
    fn ctrl_p_opens_browser_search_outside_text_entry() {
        let mut model = DashboardModel::new(curated_public_demo());
        let interaction_before = model.screen.interaction_snapshot();

        let ctrl_p = KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL);
        let _ = model.update(DashboardMessage(Event::Key(ctrl_p)));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.last_deep_link(), Some("search:"));
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
        assert_eq!(model.screen.interaction_snapshot(), interaction_before);
    }

    #[test]
    fn ctrl_p_does_not_escape_dashboard_text_entry() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.screen.set_query_interaction("active query", true);
        let interaction_before = model.screen.interaction_snapshot();

        let ctrl_p = KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL);
        let _ = model.update(DashboardMessage(Event::Key(ctrl_p)));

        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert_eq!(model.screen.interaction_snapshot(), interaction_before);
    }

    #[test]
    fn colon_remains_literal_inside_dashboard_text_entry() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.screen.set_query_interaction("active query", true);

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            ':',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert_eq!(model.screen.interaction_snapshot().query(), "active query:");
    }

    #[test]
    fn colon_opens_the_browser_search_adapter() {
        let mut model = DashboardModel::new(curated_public_demo());
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            ':',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.last_deep_link(), Some("search:"));
    }

    #[test]
    fn ctrl_p_preserves_an_active_search_editor() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.open_search("reservation");
        let revision_before = model.interaction_revision();

        let ctrl_p = KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL);
        let _ = model.update(DashboardMessage(Event::Key(ctrl_p)));

        assert_eq!(model.active_screen(), MailScreenId::Search);
        assert_eq!(model.public_screen.search_query(), "reservation");
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
        assert_eq!(model.interaction_revision(), revision_before);
    }

    #[test]
    fn same_screen_search_activation_increments_revision_exactly_once() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.open_search("first");
        let revision_before = model.interaction_revision();

        model.open_search("second");
        assert_eq!(model.public_screen.search_query(), "second");
        assert_eq!(
            model.interaction_revision(),
            revision_before.saturating_add(1)
        );

        let unchanged_revision = model.interaction_revision();
        model.open_search("second");
        assert_eq!(model.interaction_revision(), unchanged_revision);
    }

    #[test]
    fn slash_reactivates_same_screen_search_with_one_revision() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.open_search("reservation");
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Escape))));
        assert!(
            !model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
        let revision_before = model.interaction_revision();

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '/',
        )))));

        assert_eq!(model.public_screen.search_query(), "reservation");
        assert!(
            model
                .public_screen
                .consumes_text_input(MailScreenId::Search)
        );
        assert_eq!(
            model.interaction_revision(),
            revision_before.saturating_add(1)
        );
    }

    #[test]
    fn dashboard_timeline_deep_link_focuses_the_matching_public_row() {
        let mut model = DashboardModel::new(curated_public_demo());

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Enter))));

        assert_eq!(model.active_screen(), MailScreenId::Timeline);
        assert!(
            model
                .last_deep_link()
                .is_some_and(|link| link.starts_with("timeline:"))
        );
        assert!(model.public_selected_row() > 0);
    }
}
