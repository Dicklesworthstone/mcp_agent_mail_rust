//! FrankenTUI model that renders the production Agent Mail dashboard.

use ftui::Event;
use ftui_runtime::program::{Cmd, Model};

use crate::demo_pack::{DemoPack, DemoPackError, curated_public_demo};
use crate::state::TuiSharedState;
use crate::tui_screens::dashboard::DashboardScreen;
use crate::tui_screens::{DeepLinkTarget, MailScreen, MailScreenMsg};

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
    state: TuiSharedState,
    pack: DemoPack,
    next_action: usize,
    elapsed_ms: u64,
    tick_count: u64,
    paused: bool,
    reduced_motion: bool,
    last_deep_link: Option<String>,
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
            state: TuiSharedState::new(),
            pack,
            next_action: 0,
            elapsed_ms: 0,
            tick_count: 0,
            paused: false,
            reduced_motion: false,
            last_deep_link: None,
        };
        model.reset();
        model
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
        self.load_pack(DemoPack::from_json(json)?)
    }

    pub fn reset(&mut self) {
        self.screen = self.fresh_screen();
        self.pack.apply_bootstrap(&self.state);
        self.next_action = 0;
        self.elapsed_ms = 0;
        self.tick_count = 0;
        self.last_deep_link = None;
        self.state.set_elapsed_ms(0);
        self.screen.tick(0, &self.state);
    }

    /// Advance the deterministic replay clock. Returns whether visible state
    /// may have changed and therefore needs a host-driven tick/render.
    pub fn advance_replay_ms(&mut self, dt_ms: u64) -> bool {
        if self.paused || dt_ms == 0 {
            return false;
        }

        let mut remaining = dt_ms;
        loop {
            let to_end = self.pack.duration_ms.saturating_sub(self.elapsed_ms);
            let step = remaining.min(to_end);
            self.elapsed_ms = self.elapsed_ms.saturating_add(step);
            self.apply_due_actions();
            remaining = remaining.saturating_sub(step);

            if self.elapsed_ms < self.pack.duration_ms {
                break;
            }
            if !self.pack.loop_replay || remaining == 0 {
                break;
            }
            self.reset();
        }

        self.state.set_elapsed_ms(self.elapsed_ms);
        true
    }

    fn apply_due_actions(&mut self) {
        while self
            .pack
            .actions
            .get(self.next_action)
            .is_some_and(|action| action.at_ms <= self.elapsed_ms)
        {
            self.pack.apply_action(self.next_action, &self.state);
            self.next_action = self.next_action.saturating_add(1);
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.reduced_motion = reduced_motion;
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
    pub fn state(&self) -> &TuiSharedState {
        &self.state
    }

    fn capture_screen_command(&mut self, command: Cmd<MailScreenMsg>) {
        match command {
            Cmd::Msg(MailScreenMsg::DeepLink(target)) => {
                self.last_deep_link = Some(match target {
                    DeepLinkTarget::TimelineAtTime(timestamp) => {
                        format!("timeline:{timestamp}")
                    }
                    DeepLinkTarget::SearchFocused(query) => format!("search:{query}"),
                });
            }
            Cmd::Batch(commands) | Cmd::Sequence(commands) => {
                for command in commands {
                    self.capture_screen_command(command);
                }
            }
            _ => {}
        }
    }
}

impl Model for DashboardModel {
    type Message = DashboardMessage;

    fn init(&mut self) -> Cmd<Self::Message> {
        self.reset();
        Cmd::none()
    }

    fn update(&mut self, message: Self::Message) -> Cmd<Self::Message> {
        if matches!(message.0, Event::Tick) {
            self.tick_count = self.tick_count.saturating_add(1);
            self.screen.tick(self.tick_count, &self.state);
        } else {
            let command = self.screen.update(&message.0, &self.state);
            self.capture_screen_command(command);
        }
        Cmd::none()
    }

    fn view(&self, frame: &mut ftui::Frame<'_>) {
        let area = ftui::layout::Rect::new(0, 0, frame.width(), frame.height());
        self.screen.view(frame, area, &self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::DashboardModel;
    use crate::demo_pack::curated_public_demo;

    #[test]
    fn replay_reset_is_deterministic() {
        let mut model = DashboardModel::new(curated_public_demo());
        assert!(model.advance_replay_ms(10_000));
        assert_eq!(model.next_action(), 7);
        model.reset();
        assert_eq!(model.elapsed_ms(), 0);
        assert_eq!(model.next_action(), 0);
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
}
