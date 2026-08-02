//! FrankenTUI model that renders the production Agent Mail dashboard.

use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, KeyCode, KeyEventKind, MouseEventKind, Style};
use ftui_runtime::program::{Cmd, Model};

use crate::demo_pack::{DemoPack, DemoPackError, curated_public_demo};
use crate::state::TuiSharedState;
use crate::tui_hit_regions::{MouseAction, MouseDispatcher};
use crate::tui_persist::AccessibilitySettings;
use crate::tui_screens::dashboard::DashboardScreen;
use crate::tui_screens::{
    DeepLinkTarget, HelpEntry, MailScreen, MailScreenId, MailScreenMsg, PublicReplayScreen,
    screen_from_jump_key, screen_meta,
};

#[derive(Debug, Clone)]
pub struct DashboardMessage(pub Event);

impl From<Event> for DashboardMessage {
    fn from(event: Event) -> Self {
        Self(event)
    }
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
    interaction_revision: u64,
    next_action: usize,
    elapsed_ms: u64,
    tick_count: u64,
    paused: bool,
    reduced_motion: bool,
    last_deep_link: Option<String>,
    prepared: bool,
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
            interaction_revision: 0,
            next_action: 0,
            elapsed_ms: 0,
            tick_count: 0,
            paused: false,
            reduced_motion: false,
            last_deep_link: None,
            prepared: false,
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
            interaction_revision: 0,
            next_action: 0,
            elapsed_ms: 0,
            tick_count: 0,
            paused: false,
            reduced_motion: false,
            last_deep_link: None,
            prepared: false,
        }
    }

    fn fresh_screen(&self) -> DashboardScreen {
        let mut screen = DashboardScreen::new();
        screen.set_motion_preferences(self.reduced_motion, !self.reduced_motion);
        screen
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
        let preserved_screen = self.active_screen;
        let preserved_help = self.help_visible;
        let preserved_deep_link = self.last_deep_link.clone();
        let dashboard_interaction = self.screen.interaction_snapshot();
        self.screen = self.fresh_screen();
        if preserve_interaction {
            self.screen.restore_interaction(
                &dashboard_interaction.0,
                dashboard_interaction.1,
                dashboard_interaction.2,
            );
        } else {
            self.public_screen.reset();
        }
        self.pack.apply_bootstrap(&self.state);
        if preserve_interaction {
            self.active_screen = preserved_screen;
            self.help_visible = preserved_help;
            self.last_deep_link = preserved_deep_link;
        } else {
            self.active_screen = MailScreenId::Dashboard;
            self.help_visible = false;
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
        self.screen.tick(0, &self.state);
        self.screen.tick(self.tick_count, &self.state);
        self.prepared = true;
    }

    /// Advance the deterministic replay clock. Returns whether visible state
    /// may have changed and therefore needs a host-driven tick/render.
    pub fn advance_replay_ms(&mut self, dt_ms: u64) -> bool {
        if self.paused || dt_ms == 0 {
            return false;
        }

        let previous_clock_second = self.elapsed_ms / 1_000;
        let mut actions_applied = false;
        let mut replay_reset = false;
        let mut remaining = dt_ms;
        loop {
            let to_end = self.pack.duration_ms.saturating_sub(self.elapsed_ms);
            let step = remaining.min(to_end);
            self.elapsed_ms = self.elapsed_ms.saturating_add(step);
            actions_applied |= self.apply_due_actions();
            remaining = remaining.saturating_sub(step);

            if self.elapsed_ms < self.pack.duration_ms {
                break;
            }
            if !self.pack.loop_replay || remaining == 0 {
                break;
            }
            self.reset_replay(true);
            replay_reset = true;
        }

        self.state.set_elapsed_ms(self.elapsed_ms);
        actions_applied || replay_reset || self.elapsed_ms / 1_000 != previous_clock_second
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

    fn capture_screen_command(&mut self, command: Cmd<MailScreenMsg>) {
        match command {
            Cmd::Msg(MailScreenMsg::DeepLink(target)) => {
                let label = match target {
                    DeepLinkTarget::TimelineAtTime(timestamp) => {
                        self.activate_screen(MailScreenId::Timeline);
                        format!("timeline:{timestamp}")
                    }
                    DeepLinkTarget::SearchFocused(query) => {
                        self.activate_screen(MailScreenId::Search);
                        self.public_screen.begin_search(&query);
                        format!("search:{query}")
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
            self.active_screen = screen;
            self.public_screen.reset();
            self.interaction_revision = self.interaction_revision.saturating_add(1);
        }
    }

    fn handle_shell_input(&mut self, event: &Event) -> bool {
        if self.help_visible {
            match event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && matches!(
                            key.code,
                            KeyCode::Escape | KeyCode::F(1) | KeyCode::Char('?')
                        ) =>
                {
                    self.help_visible = false;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)) => {
                    self.help_visible = false;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                _ => return true,
            }
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
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                MouseAction::OpenPalette => {
                    self.activate_screen(MailScreenId::Search);
                    self.public_screen.begin_search("");
                    self.last_deep_link = Some("search:".to_string());
                    return true;
                }
                MouseAction::Forward => {}
            }
        }

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
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
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                KeyCode::Char('?') if !text_mode => {
                    self.help_visible = true;
                    self.interaction_revision = self.interaction_revision.saturating_add(1);
                    return true;
                }
                KeyCode::Char('/') if !text_mode => {
                    self.last_deep_link = Some("search:".to_string());
                    self.activate_screen(MailScreenId::Search);
                    self.public_screen.begin_search("");
                    return true;
                }
                KeyCode::Char(character) if !text_mode => {
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
            self.screen
                .keybindings()
                .into_iter()
                .filter(|entry| !matches!(entry.key, "/" | "1-4"))
                .collect()
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
            self.tick_count = self.tick_count.saturating_add(1);
            self.screen.tick(self.tick_count, &self.state);
        } else if self.handle_shell_input(&message.0) {
            return Cmd::none();
        } else if self.active_screen == MailScreenId::Dashboard {
            let command = self.screen.update(&message.0, &self.state);
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
        let area = ftui::layout::Rect::new(0, 0, frame.width(), frame.height());
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
        crate::tui_chrome::render_status_line(
            &self.state,
            self.active_screen,
            "REPLAY",
            false,
            self.help_visible,
            &self.accessibility,
            &bindings,
            false,
            frame,
            chrome.status_line,
        );

        if self.help_visible {
            render_browser_help(self.active_screen, frame, area);
        }
    }
}

fn render_browser_help(
    screen: MailScreenId,
    frame: &mut ftui::Frame<'_>,
    area: ftui::layout::Rect,
) {
    let palette = crate::tui_theme::TuiThemePalette::current();
    let meta = screen_meta(screen);
    let overlay = crate::tui_chrome::help_overlay_rect(area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Agent Mail Browser Controls · F1/Esc close ")
        .style(
            Style::default()
                .fg(palette.help_border_fg)
                .bg(palette.help_bg),
        );
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    let text = format!(
        "{}\n{}\n\nMouse\n  Click a top tab to switch screens\n  Click Dashboard filters or public replay rows\n  Scroll inside the active panel\n\nKeyboard\n  Tab / Shift+Tab     next / previous screen\n  1-9, 0, ! through ^ direct screen jump\n  /                    open Search\n  F1 or ?              toggle this help\n\nThis browser build is read-only. Aggregate counts come from a read-only Agent Mail SQLite export; names, paths, messages, and replay events are synthetic public-demo details.",
        meta.title, meta.description
    );
    Paragraph::new(text)
        .style(Style::default().fg(palette.help_fg).bg(palette.help_bg))
        .render(inner, frame);
}

#[cfg(test)]
mod tests {
    use ftui::{Event, KeyCode, KeyEvent};
    use ftui_runtime::program::Model;

    use super::{DashboardMessage, DashboardModel};
    use crate::demo_pack::curated_public_demo;
    use crate::tui_screens::MailScreenId;

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
        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '2',
        )))));
        assert_eq!(model.active_screen(), MailScreenId::Messages);

        assert!(model.advance_replay_ms(model.pack().duration_ms + 100));

        assert_eq!(model.active_screen(), MailScreenId::Messages);
        assert_eq!(model.elapsed_ms(), 100);
    }

    #[test]
    fn dashboard_text_mode_receives_digits_instead_of_switching_tabs() {
        let mut model = DashboardModel::new(curated_public_demo());
        model.screen.restore_interaction("", true, "all");

        let _ = model.update(DashboardMessage(Event::Key(KeyEvent::new(KeyCode::Char(
            '2',
        )))));

        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert_eq!(model.screen.interaction_snapshot().0, "2");
    }

    #[test]
    fn slash_opens_an_editable_public_search() {
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
}
