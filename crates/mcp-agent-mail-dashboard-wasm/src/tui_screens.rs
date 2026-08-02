//! Portable screen contract required by the shared dashboard source.

use ftui::Event;
use ftui::layout::Rect;
use ftui_runtime::program::Cmd;

use crate::state::TuiSharedState;

#[derive(Debug, Clone)]
pub struct HelpEntry {
    pub key: &'static str,
    pub action: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataGeneration {
    pub event_total_pushed: u64,
    pub console_log_seq: u64,
    pub db_stats_gen: u64,
    pub request_gen: u64,
}

impl DataGeneration {
    #[must_use]
    pub const fn stale() -> Self {
        Self {
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
        events: current.event_total_pushed != previous.event_total_pushed,
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

#[path = "../../mcp-agent-mail-server/src/tui_screens/dashboard.rs"]
pub mod dashboard;
