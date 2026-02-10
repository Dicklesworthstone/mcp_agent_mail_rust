//! Top-level TUI application model for `AgentMailTUI`.
//!
//! [`MailAppModel`] implements the `ftui_runtime` [`Model`] trait,
//! orchestrating screen switching, global keybindings, tick dispatch,
//! and shared-state access.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use ftui::Frame;
use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::command_palette::{ActionItem, CommandPalette, PaletteAction};
use ftui::widgets::notification_queue::NotificationStack;
use ftui::widgets::{NotificationQueue, QueueConfig, Toast, ToastIcon};
use ftui::{Event, KeyCode, KeyEventKind, Modifiers};
use ftui_runtime::program::{Cmd, Model};

use crate::tui_bridge::{ServerControlMsg, TransportBase, TuiSharedState};
use crate::tui_events::MailEvent;
use crate::tui_screens::{
    ALL_SCREEN_IDS, DeepLinkTarget, MAIL_SCREEN_REGISTRY, MailScreen, MailScreenId, MailScreenMsg,
    agents::AgentsScreen, analytics::AnalyticsScreen, contacts::ContactsScreen,
    dashboard::DashboardScreen, explorer::MailExplorerScreen, messages::MessageBrowserScreen,
    projects::ProjectsScreen, reservations::ReservationsScreen, screen_meta,
    search::SearchCockpitScreen, system_health::SystemHealthScreen, threads::ThreadExplorerScreen,
    timeline::TimelineScreen, tool_metrics::ToolMetricsScreen,
};

/// How often the TUI ticks (100 ms ≈ 10 fps).
const TICK_INTERVAL: Duration = Duration::from_millis(100);

const PALETTE_MAX_VISIBLE: usize = 12;
const PALETTE_DYNAMIC_AGENT_CAP: usize = 50;
const PALETTE_DYNAMIC_THREAD_CAP: usize = 50;
const PALETTE_DYNAMIC_TOOL_CAP: usize = 50;
const PALETTE_DYNAMIC_PROJECT_CAP: usize = 30;
const PALETTE_DYNAMIC_CONTACT_CAP: usize = 30;
const PALETTE_DYNAMIC_RESERVATION_CAP: usize = 30;
const PALETTE_DYNAMIC_EVENT_SCAN: usize = 1500;

// ──────────────────────────────────────────────────────────────────────
// MailMsg — top-level message type
// ──────────────────────────────────────────────────────────────────────

/// Top-level message type for the TUI application.
#[derive(Debug, Clone)]
pub enum MailMsg {
    /// Terminal event (keyboard, mouse, resize, tick).
    Terminal(Event),
    /// Forwarded screen-level message.
    Screen(MailScreenMsg),
    /// Switch to a specific screen.
    SwitchScreen(MailScreenId),
    /// Toggle the help overlay.
    ToggleHelp,
    /// Request application quit.
    Quit,
}

impl From<Event> for MailMsg {
    fn from(event: Event) -> Self {
        Self::Terminal(event)
    }
}

// ──────────────────────────────────────────────────────────────────────
// MailAppModel — implements ftui_runtime::Model
// ──────────────────────────────────────────────────────────────────────

/// The top-level TUI application model.
///
/// Owns all screen instances and dispatches events to the active screen
/// after processing global keybindings.
pub struct MailAppModel {
    state: Arc<TuiSharedState>,
    active_screen: MailScreenId,
    screens: HashMap<MailScreenId, Box<dyn MailScreen>>,
    help_visible: bool,
    help_scroll: u16,
    keymap: crate::tui_keymap::KeymapRegistry,
    command_palette: CommandPalette,
    notifications: NotificationQueue,
    last_toast_seq: u64,
    tick_count: u64,
    accessibility: crate::tui_persist::AccessibilitySettings,
}

impl MailAppModel {
    /// Create a new application model with placeholder screens (no persistence).
    #[must_use]
    pub fn new(state: Arc<TuiSharedState>) -> Self {
        let mut screens: HashMap<MailScreenId, Box<dyn MailScreen>> = HashMap::new();
        for &id in ALL_SCREEN_IDS {
            if id == MailScreenId::Dashboard {
                screens.insert(id, Box::new(DashboardScreen::new()));
            } else if id == MailScreenId::Messages {
                screens.insert(id, Box::new(MessageBrowserScreen::new()));
            } else if id == MailScreenId::Threads {
                screens.insert(id, Box::new(ThreadExplorerScreen::new()));
            } else if id == MailScreenId::Timeline {
                screens.insert(id, Box::new(TimelineScreen::new()));
            } else if id == MailScreenId::SystemHealth {
                screens.insert(id, Box::new(SystemHealthScreen::new(Arc::clone(&state))));
            } else if id == MailScreenId::Agents {
                screens.insert(id, Box::new(AgentsScreen::new()));
            } else if id == MailScreenId::Search {
                screens.insert(id, Box::new(SearchCockpitScreen::new()));
            } else if id == MailScreenId::ToolMetrics {
                screens.insert(id, Box::new(ToolMetricsScreen::new()));
            } else if id == MailScreenId::Reservations {
                screens.insert(id, Box::new(ReservationsScreen::new()));
            } else if id == MailScreenId::Projects {
                screens.insert(id, Box::new(ProjectsScreen::new()));
            } else if id == MailScreenId::Contacts {
                screens.insert(id, Box::new(ContactsScreen::new()));
            } else if id == MailScreenId::Explorer {
                screens.insert(id, Box::new(MailExplorerScreen::new()));
            } else if id == MailScreenId::Analytics {
                screens.insert(id, Box::new(AnalyticsScreen::new()));
            }
        }
        let mut command_palette = CommandPalette::new().with_max_visible(PALETTE_MAX_VISIBLE);
        command_palette.replace_actions(build_palette_actions_static());
        Self {
            state,
            active_screen: MailScreenId::Dashboard,
            screens,
            help_visible: false,
            help_scroll: 0,
            keymap: crate::tui_keymap::KeymapRegistry::default(),
            command_palette,
            notifications: NotificationQueue::new(QueueConfig::default()),
            last_toast_seq: 0,
            tick_count: 0,
            accessibility: crate::tui_persist::AccessibilitySettings::default(),
        }
    }

    /// Create the model with config-driven preferences and auto-persistence.
    #[must_use]
    pub fn with_config(state: Arc<TuiSharedState>, config: &mcp_agent_mail_core::Config) -> Self {
        let mut model = Self::new(state);
        // Load accessibility settings from config.
        model.accessibility = crate::tui_persist::AccessibilitySettings {
            high_contrast: config.tui_high_contrast,
            key_hints: config.tui_key_hints,
        };
        // Restore keymap profile from persisted config.
        let prefs = crate::tui_persist::TuiPreferences::from_config(config);
        model.keymap.set_profile(prefs.keymap_profile);
        model
    }

    /// Replace a screen implementation (used when real screens are ready).
    pub fn set_screen(&mut self, id: MailScreenId, screen: Box<dyn MailScreen>) {
        self.screens.insert(id, screen);
    }

    /// Get the currently active screen ID.
    #[must_use]
    pub const fn active_screen(&self) -> MailScreenId {
        self.active_screen
    }

    /// Whether the help overlay is currently shown.
    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// Current accessibility settings.
    #[must_use]
    pub const fn accessibility(&self) -> &crate::tui_persist::AccessibilitySettings {
        &self.accessibility
    }

    /// Mutable access to the keymap registry.
    pub const fn keymap_mut(&mut self) -> &mut crate::tui_keymap::KeymapRegistry {
        &mut self.keymap
    }

    /// Read-only access to the keymap registry.
    #[must_use]
    pub const fn keymap(&self) -> &crate::tui_keymap::KeymapRegistry {
        &self.keymap
    }

    /// Whether the active screen is consuming text input.
    fn consumes_text_input(&self) -> bool {
        if self.command_palette.is_visible() {
            return true;
        }
        self.screens
            .get(&self.active_screen)
            .is_some_and(|s| s.consumes_text_input())
    }

    fn open_palette(&mut self) {
        self.help_visible = false;
        let mut actions = build_palette_actions(&self.state);

        // Inject context-aware quick actions from the focused entity.
        if let Some(screen) = self.screens.get(&self.active_screen) {
            if let Some(event) = screen.focused_event() {
                let quick = crate::tui_screens::inspector::build_quick_actions(event);
                for qa in quick.into_iter().rev() {
                    actions.insert(
                        0,
                        ActionItem::new(qa.id, qa.label)
                            .with_description(&qa.description)
                            .with_tags(&["quick", "context"])
                            .with_category("Quick Actions"),
                    );
                }
            }
        }

        self.command_palette.replace_actions(actions);
        self.command_palette.open();
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_palette_action(&mut self, id: &str) -> Cmd<MailMsg> {
        // ── App controls ───────────────────────────────────────────
        match id {
            palette_action_ids::APP_TOGGLE_HELP => {
                self.help_visible = !self.help_visible;
                self.help_scroll = 0;
                return Cmd::none();
            }
            palette_action_ids::APP_QUIT => {
                self.state.request_shutdown();
                return Cmd::quit();
            }
            palette_action_ids::TRANSPORT_TOGGLE => {
                let _ = self
                    .state
                    .try_send_server_control(ServerControlMsg::ToggleTransportBase);
                return Cmd::none();
            }
            palette_action_ids::TRANSPORT_SET_MCP => {
                let _ = self
                    .state
                    .try_send_server_control(ServerControlMsg::SetTransportBase(
                        TransportBase::Mcp,
                    ));
                return Cmd::none();
            }
            palette_action_ids::TRANSPORT_SET_API => {
                let _ = self
                    .state
                    .try_send_server_control(ServerControlMsg::SetTransportBase(
                        TransportBase::Api,
                    ));
                return Cmd::none();
            }
            palette_action_ids::LAYOUT_RESET => {
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Threads) {
                    screen.reset_layout();
                }
                return Cmd::none();
            }
            palette_action_ids::LAYOUT_EXPORT => {
                if let Some(screen) = self.screens.get(&MailScreenId::Threads) {
                    screen.export_layout();
                }
                return Cmd::none();
            }
            palette_action_ids::LAYOUT_IMPORT => {
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Threads) {
                    screen.import_layout();
                }
                return Cmd::none();
            }
            palette_action_ids::A11Y_TOGGLE_HC => {
                self.accessibility.high_contrast = !self.accessibility.high_contrast;
                return Cmd::none();
            }
            palette_action_ids::A11Y_TOGGLE_HINTS => {
                self.accessibility.key_hints = !self.accessibility.key_hints;
                return Cmd::none();
            }
            palette_action_ids::THEME_CYCLE => {
                let name = crate::tui_theme::cycle_and_get_name();
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            _ => {}
        }

        // ── Screen navigation ─────────────────────────────────────
        if let Some(screen_id) = screen_from_palette_action_id(id) {
            self.active_screen = screen_id;
            return Cmd::none();
        }

        // ── Dynamic sources ───────────────────────────────────────
        if id.starts_with(palette_action_ids::AGENT_PREFIX) {
            self.active_screen = MailScreenId::Agents;
            return Cmd::none();
        }
        if id.starts_with(palette_action_ids::THREAD_PREFIX) {
            self.active_screen = MailScreenId::Threads;
            return Cmd::none();
        }
        if id.starts_with(palette_action_ids::TOOL_PREFIX) {
            self.active_screen = MailScreenId::ToolMetrics;
            return Cmd::none();
        }
        if let Some(slug) = id.strip_prefix(palette_action_ids::PROJECT_PREFIX) {
            let target = DeepLinkTarget::ProjectBySlug(slug.to_string());
            self.active_screen = MailScreenId::Projects;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Projects) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }
        if let Some(pair) = id.strip_prefix(palette_action_ids::CONTACT_PREFIX) {
            if let Some((from, to)) = pair.split_once(':') {
                let target = DeepLinkTarget::ContactByPair(from.to_string(), to.to_string());
                self.active_screen = MailScreenId::Contacts;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Contacts) {
                    screen.receive_deep_link(&target);
                }
            } else {
                self.active_screen = MailScreenId::Contacts;
            }
            return Cmd::none();
        }
        if let Some(agent) = id.strip_prefix(palette_action_ids::RESERVATION_PREFIX) {
            let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
            self.active_screen = MailScreenId::Reservations;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Reservations) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }

        // ── Quick actions (context-aware from focused entity) ────
        if let Some(rest) = id.strip_prefix("quick:") {
            if let Some(name) = rest.strip_prefix("agent:") {
                let target = DeepLinkTarget::AgentByName(name.to_string());
                self.active_screen = MailScreenId::Agents;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Agents) {
                    screen.receive_deep_link(&target);
                }
                return Cmd::none();
            }
            if let Some(id_str) = rest.strip_prefix("thread:") {
                let target = DeepLinkTarget::ThreadById(id_str.to_string());
                self.active_screen = MailScreenId::Threads;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Threads) {
                    screen.receive_deep_link(&target);
                }
                return Cmd::none();
            }
            if let Some(name) = rest.strip_prefix("tool:") {
                let target = DeepLinkTarget::ToolByName(name.to_string());
                self.active_screen = MailScreenId::ToolMetrics;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::ToolMetrics) {
                    screen.receive_deep_link(&target);
                }
                return Cmd::none();
            }
            if let Some(id_str) = rest.strip_prefix("message:") {
                if let Ok(msg_id) = id_str.parse::<i64>() {
                    let target = DeepLinkTarget::MessageById(msg_id);
                    self.active_screen = MailScreenId::Messages;
                    if let Some(screen) = self.screens.get_mut(&MailScreenId::Messages) {
                        screen.receive_deep_link(&target);
                    }
                }
                return Cmd::none();
            }
            if let Some(slug) = rest.strip_prefix("project:") {
                let target = DeepLinkTarget::ProjectBySlug(slug.to_string());
                self.active_screen = MailScreenId::Projects;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Projects) {
                    screen.receive_deep_link(&target);
                }
                return Cmd::none();
            }
            if let Some(agent) = rest.strip_prefix("reservation:") {
                let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
                self.active_screen = MailScreenId::Reservations;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Reservations) {
                    screen.receive_deep_link(&target);
                }
                return Cmd::none();
            }
        }

        Cmd::none()
    }
}

impl Model for MailAppModel {
    type Message = MailMsg;

    fn init(&mut self) -> Cmd<Self::Message> {
        Cmd::batch(vec![Cmd::tick(TICK_INTERVAL), Cmd::set_mouse_capture(true)])
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, msg: Self::Message) -> Cmd<Self::Message> {
        match msg {
            // ── Tick ────────────────────────────────────────────────
            MailMsg::Terminal(Event::Tick) => {
                self.tick_count += 1;
                for screen in self.screens.values_mut() {
                    screen.tick(self.tick_count, &self.state);
                }

                // Generate toasts from new high-priority events
                let new_events = self.state.events_since(self.last_toast_seq);
                for event in &new_events {
                    self.last_toast_seq = event.seq().max(self.last_toast_seq);
                    if let Some(toast) = toast_for_event(event) {
                        self.notifications.notify(toast);
                    }
                }

                // Advance notification timers
                self.notifications.tick(TICK_INTERVAL);

                Cmd::tick(TICK_INTERVAL)
            }

            // ── Terminal events (key, mouse, resize, etc.) ─────────
            MailMsg::Terminal(ref event) => {
                // When the command palette is visible, route all events to it first.
                if self.command_palette.is_visible() {
                    if let Some(action) = self.command_palette.handle_event(event) {
                        match action {
                            PaletteAction::Execute(id) => return self.dispatch_palette_action(&id),
                            PaletteAction::Dismiss => {}
                        }
                    }
                    return Cmd::none();
                }

                // Global keybindings (checked before screen dispatch)
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        let text_mode = self.consumes_text_input();
                        let is_ctrl_p = key.modifiers.contains(Modifiers::CTRL)
                            && matches!(key.code, KeyCode::Char('p'));
                        if (is_ctrl_p || matches!(key.code, KeyCode::Char(':'))) && !text_mode {
                            self.open_palette();
                            return Cmd::none();
                        }
                        match key.code {
                            KeyCode::Char('q') if !text_mode => {
                                self.state.request_shutdown();
                                return Cmd::quit();
                            }
                            KeyCode::Char('?') if !text_mode => {
                                self.help_visible = !self.help_visible;
                                self.help_scroll = 0;
                                return Cmd::none();
                            }
                            KeyCode::Char('m') if !text_mode => {
                                let _ = self
                                    .state
                                    .try_send_server_control(ServerControlMsg::ToggleTransportBase);
                                return Cmd::none();
                            }
                            KeyCode::Char('T') if !text_mode => {
                                let name = crate::tui_theme::cycle_and_get_name();
                                self.notifications.notify(
                                    Toast::new(format!("Theme: {name}"))
                                        .icon(ToastIcon::Info)
                                        .duration(Duration::from_secs(3)),
                                );
                                return Cmd::none();
                            }
                            KeyCode::Tab => {
                                self.active_screen = self.active_screen.next();
                                return Cmd::none();
                            }
                            KeyCode::BackTab => {
                                self.active_screen = self.active_screen.prev();
                                return Cmd::none();
                            }
                            KeyCode::Escape if self.help_visible => {
                                self.help_visible = false;
                                return Cmd::none();
                            }
                            // Scroll help overlay with j/k or arrow keys.
                            KeyCode::Char('j') | KeyCode::Down if self.help_visible => {
                                self.help_scroll = self.help_scroll.saturating_add(1);
                                return Cmd::none();
                            }
                            KeyCode::Char('k') | KeyCode::Up if self.help_visible => {
                                self.help_scroll = self.help_scroll.saturating_sub(1);
                                return Cmd::none();
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() && !text_mode => {
                                let n = c.to_digit(10).unwrap_or(0) as usize;
                                if let Some(id) = MailScreenId::from_number(n) {
                                    self.active_screen = id;
                                    return Cmd::none();
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Forward unhandled events to the active screen
                if let Some(screen) = self.screens.get_mut(&self.active_screen) {
                    map_screen_cmd(screen.update(event, &self.state))
                } else {
                    Cmd::none()
                }
            }

            // ── Screen messages / direct navigation ─────────────────
            MailMsg::Screen(MailScreenMsg::Navigate(id)) | MailMsg::SwitchScreen(id) => {
                self.active_screen = id;
                Cmd::none()
            }
            MailMsg::Screen(MailScreenMsg::Noop) => Cmd::none(),
            MailMsg::Screen(MailScreenMsg::DeepLink(ref target)) => {
                // Route deep-link to the appropriate screen.
                use crate::tui_screens::DeepLinkTarget;
                let target_screen = match target {
                    DeepLinkTarget::TimelineAtTime(_) => MailScreenId::Timeline,
                    DeepLinkTarget::ThreadById(_) => MailScreenId::Threads,
                    DeepLinkTarget::MessageById(_) => MailScreenId::Messages,
                    DeepLinkTarget::AgentByName(_) => MailScreenId::Agents,
                    DeepLinkTarget::ToolByName(_) => MailScreenId::ToolMetrics,
                    DeepLinkTarget::ProjectBySlug(_) => MailScreenId::Projects,
                    DeepLinkTarget::ReservationByAgent(_) => MailScreenId::Reservations,
                    DeepLinkTarget::ContactByPair(_, _) => MailScreenId::Contacts,
                    DeepLinkTarget::ExplorerForAgent(_) => MailScreenId::Explorer,
                };
                self.active_screen = target_screen;
                if let Some(screen) = self.screens.get_mut(&target_screen) {
                    screen.receive_deep_link(target);
                }
                Cmd::none()
            }
            MailMsg::ToggleHelp => {
                self.help_visible = !self.help_visible;
                self.help_scroll = 0;
                Cmd::none()
            }
            MailMsg::Quit => {
                self.state.request_shutdown();
                Cmd::quit()
            }
        }
    }

    fn view(&self, frame: &mut Frame) {
        use crate::tui_chrome;

        let area = Rect::new(0, 0, frame.width(), frame.height());
        let chrome = tui_chrome::chrome_layout(area);

        // 1. Tab bar (z=1)
        tui_chrome::render_tab_bar(self.active_screen, frame, chrome.tab_bar);

        // 2. Screen content (z=2)
        if let Some(screen) = self.screens.get(&self.active_screen) {
            screen.view(frame, chrome.content, &self.state);
        }

        // 3. Status line (z=3)
        tui_chrome::render_status_line(
            &self.state,
            self.active_screen,
            self.help_visible,
            frame,
            chrome.status_line,
        );

        // 4. Toast notifications (z=4, overlay)
        NotificationStack::new(&self.notifications)
            .margin(1)
            .render(area, frame);

        // 5. Command palette (z=5, modal)
        if self.command_palette.is_visible() {
            self.command_palette.render(area, frame);
        }

        // 6. Help overlay (z=6, topmost)
        if self.help_visible {
            let screen_bindings = self
                .screens
                .get(&self.active_screen)
                .map(|s| s.keybindings())
                .unwrap_or_default();
            let screen_label = crate::tui_screens::screen_meta(self.active_screen).title;
            let sections = self.keymap.contextual_help(&screen_bindings, screen_label);
            tui_chrome::render_help_overlay_sections(&sections, self.help_scroll, frame, area);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cmd mapping helper
// ──────────────────────────────────────────────────────────────────────

/// Map a `Cmd<MailScreenMsg>` into a `Cmd<MailMsg>`.
fn map_screen_cmd(cmd: Cmd<MailScreenMsg>) -> Cmd<MailMsg> {
    match cmd {
        Cmd::None => Cmd::none(),
        Cmd::Quit => Cmd::quit(),
        Cmd::Msg(m) => Cmd::msg(MailMsg::Screen(m)),
        Cmd::Tick(d) => Cmd::tick(d),
        Cmd::Log(s) => Cmd::log(s),
        Cmd::Batch(cmds) => Cmd::batch(cmds.into_iter().map(map_screen_cmd).collect()),
        Cmd::Sequence(cmds) => Cmd::sequence(cmds.into_iter().map(map_screen_cmd).collect()),
        Cmd::SaveState => Cmd::save_state(),
        Cmd::RestoreState => Cmd::restore_state(),
        Cmd::SetMouseCapture(b) => Cmd::set_mouse_capture(b),
        Cmd::Task(spec, f) => Cmd::Task(spec, Box::new(move || MailMsg::Screen(f()))),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Command palette catalog
// ──────────────────────────────────────────────────────────────────────

mod palette_action_ids {
    pub const APP_TOGGLE_HELP: &str = "app:toggle_help";
    pub const APP_QUIT: &str = "app:quit";

    pub const TRANSPORT_TOGGLE: &str = "transport:toggle";
    pub const TRANSPORT_SET_MCP: &str = "transport:set_mcp";
    pub const TRANSPORT_SET_API: &str = "transport:set_api";

    pub const LAYOUT_RESET: &str = "layout:reset";
    pub const LAYOUT_EXPORT: &str = "layout:export";
    pub const LAYOUT_IMPORT: &str = "layout:import";

    pub const A11Y_TOGGLE_HC: &str = "a11y:toggle_high_contrast";
    pub const A11Y_TOGGLE_HINTS: &str = "a11y:toggle_key_hints";

    pub const THEME_CYCLE: &str = "theme:cycle";

    pub const AGENT_PREFIX: &str = "agent:";
    pub const THREAD_PREFIX: &str = "thread:";
    pub const TOOL_PREFIX: &str = "tool:";
    pub const PROJECT_PREFIX: &str = "project:";
    pub const CONTACT_PREFIX: &str = "contact:";
    pub const RESERVATION_PREFIX: &str = "reservation:";

    pub const SCREEN_DASHBOARD: &str = "screen:dashboard";
    pub const SCREEN_MESSAGES: &str = "screen:messages";
    pub const SCREEN_THREADS: &str = "screen:threads";
    pub const SCREEN_TIMELINE: &str = "screen:timeline";
    pub const SCREEN_AGENTS: &str = "screen:agents";
    pub const SCREEN_RESERVATIONS: &str = "screen:reservations";
    pub const SCREEN_TOOL_METRICS: &str = "screen:tool_metrics";
    pub const SCREEN_SYSTEM_HEALTH: &str = "screen:system_health";
    pub const SCREEN_SEARCH: &str = "screen:search";
    pub const SCREEN_PROJECTS: &str = "screen:projects";
    pub const SCREEN_CONTACTS: &str = "screen:contacts";
    pub const SCREEN_EXPLORER: &str = "screen:explorer";
    pub const SCREEN_ANALYTICS: &str = "screen:analytics";
}

fn screen_from_palette_action_id(id: &str) -> Option<MailScreenId> {
    match id {
        palette_action_ids::SCREEN_DASHBOARD => Some(MailScreenId::Dashboard),
        palette_action_ids::SCREEN_MESSAGES => Some(MailScreenId::Messages),
        palette_action_ids::SCREEN_THREADS => Some(MailScreenId::Threads),
        palette_action_ids::SCREEN_TIMELINE => Some(MailScreenId::Timeline),
        palette_action_ids::SCREEN_AGENTS => Some(MailScreenId::Agents),
        palette_action_ids::SCREEN_SEARCH => Some(MailScreenId::Search),
        palette_action_ids::SCREEN_RESERVATIONS => Some(MailScreenId::Reservations),
        palette_action_ids::SCREEN_TOOL_METRICS => Some(MailScreenId::ToolMetrics),
        palette_action_ids::SCREEN_SYSTEM_HEALTH => Some(MailScreenId::SystemHealth),
        palette_action_ids::SCREEN_PROJECTS => Some(MailScreenId::Projects),
        palette_action_ids::SCREEN_CONTACTS => Some(MailScreenId::Contacts),
        palette_action_ids::SCREEN_EXPLORER => Some(MailScreenId::Explorer),
        palette_action_ids::SCREEN_ANALYTICS => Some(MailScreenId::Analytics),
        _ => None,
    }
}

const fn screen_palette_action_id(id: MailScreenId) -> &'static str {
    match id {
        MailScreenId::Dashboard => palette_action_ids::SCREEN_DASHBOARD,
        MailScreenId::Messages => palette_action_ids::SCREEN_MESSAGES,
        MailScreenId::Threads => palette_action_ids::SCREEN_THREADS,
        MailScreenId::Timeline => palette_action_ids::SCREEN_TIMELINE,
        MailScreenId::Agents => palette_action_ids::SCREEN_AGENTS,
        MailScreenId::Search => palette_action_ids::SCREEN_SEARCH,
        MailScreenId::Reservations => palette_action_ids::SCREEN_RESERVATIONS,
        MailScreenId::ToolMetrics => palette_action_ids::SCREEN_TOOL_METRICS,
        MailScreenId::SystemHealth => palette_action_ids::SCREEN_SYSTEM_HEALTH,
        MailScreenId::Projects => palette_action_ids::SCREEN_PROJECTS,
        MailScreenId::Contacts => palette_action_ids::SCREEN_CONTACTS,
        MailScreenId::Explorer => palette_action_ids::SCREEN_EXPLORER,
        MailScreenId::Analytics => palette_action_ids::SCREEN_ANALYTICS,
    }
}

fn screen_palette_category(id: MailScreenId) -> &'static str {
    match screen_meta(id).category {
        crate::tui_screens::ScreenCategory::Overview => "Navigate",
        crate::tui_screens::ScreenCategory::Communication => "Communication",
        crate::tui_screens::ScreenCategory::Operations => "Operations",
        crate::tui_screens::ScreenCategory::System => "Diagnostics",
    }
}

#[must_use]
fn build_palette_actions_static() -> Vec<ActionItem> {
    let mut out = Vec::with_capacity(MAIL_SCREEN_REGISTRY.len() + 2);

    for meta in MAIL_SCREEN_REGISTRY {
        out.push(
            ActionItem::new(
                screen_palette_action_id(meta.id),
                format!("Go to {}", meta.title),
            )
            .with_description(meta.description)
            .with_tags(&["screen", "navigate"])
            .with_category(screen_palette_category(meta.id)),
        );
    }

    out.push(
        ActionItem::new(palette_action_ids::TRANSPORT_TOGGLE, "Toggle MCP/API Mode")
            .with_description("Restart server to switch between /mcp/ and /api/ base paths")
            .with_tags(&["transport", "mode", "mcp", "api"])
            .with_category("Transport"),
    );
    out.push(
        ActionItem::new(palette_action_ids::TRANSPORT_SET_MCP, "Switch to MCP Mode")
            .with_description("Restart server with /mcp/ base path")
            .with_tags(&["transport", "mcp"])
            .with_category("Transport"),
    );
    out.push(
        ActionItem::new(palette_action_ids::TRANSPORT_SET_API, "Switch to API Mode")
            .with_description("Restart server with /api/ base path")
            .with_tags(&["transport", "api"])
            .with_category("Transport"),
    );

    out.push(
        ActionItem::new(palette_action_ids::LAYOUT_RESET, "Reset Layout")
            .with_description("Reset dock layout to factory defaults (Right 40%)")
            .with_tags(&["layout", "reset", "defaults", "dock"])
            .with_category("Layout"),
    );
    out.push(
        ActionItem::new(palette_action_ids::LAYOUT_EXPORT, "Export Layout")
            .with_description("Save current dock layout to layout.json")
            .with_tags(&["layout", "export", "save", "json"])
            .with_category("Layout"),
    );
    out.push(
        ActionItem::new(palette_action_ids::LAYOUT_IMPORT, "Import Layout")
            .with_description("Load dock layout from layout.json")
            .with_tags(&["layout", "import", "load", "json"])
            .with_category("Layout"),
    );

    out.push(
        ActionItem::new(palette_action_ids::THEME_CYCLE, "Cycle Theme")
            .with_description("Switch to the next color theme (T)")
            .with_tags(&["theme", "colors", "appearance"])
            .with_category("Appearance"),
    );

    out.push(
        ActionItem::new(palette_action_ids::A11Y_TOGGLE_HC, "Toggle High Contrast")
            .with_description("Switch between standard and high-contrast color palette")
            .with_tags(&["accessibility", "contrast", "colors", "a11y"])
            .with_category("Accessibility"),
    );
    out.push(
        ActionItem::new(palette_action_ids::A11Y_TOGGLE_HINTS, "Toggle Key Hints")
            .with_description("Show/hide context-sensitive key hints in the status area")
            .with_tags(&["accessibility", "hints", "keys", "a11y"])
            .with_category("Accessibility"),
    );

    out.push(
        ActionItem::new(palette_action_ids::APP_TOGGLE_HELP, "Toggle Help Overlay")
            .with_description("Show/hide the keybinding reference")
            .with_tags(&["help", "keys"])
            .with_category("App"),
    );
    out.push(
        ActionItem::new(palette_action_ids::APP_QUIT, "Quit")
            .with_description("Exit AgentMailTUI (requests shutdown)")
            .with_tags(&["quit", "exit"])
            .with_category("App"),
    );

    out
}

#[must_use]
fn build_palette_actions(state: &TuiSharedState) -> Vec<ActionItem> {
    let mut out = build_palette_actions_static();
    build_palette_actions_from_snapshot(state, &mut out);
    build_palette_actions_from_events(state, &mut out);
    out
}

/// Append palette entries derived from the periodic DB snapshot (agents, projects, contacts).
fn build_palette_actions_from_snapshot(state: &TuiSharedState, out: &mut Vec<ActionItem>) {
    let Some(snap) = state.db_stats_snapshot() else {
        return;
    };

    for agent in snap.agents_list.into_iter().take(PALETTE_DYNAMIC_AGENT_CAP) {
        let crate::tui_events::AgentSummary {
            name,
            program,
            last_active_ts,
        } = agent;
        let desc = format!("{program} (last_active_ts: {last_active_ts})");
        out.push(
            ActionItem::new(
                format!("{}{}", palette_action_ids::AGENT_PREFIX, name),
                format!("Agent: {name}"),
            )
            .with_description(desc)
            .with_tags(&["agent"])
            .with_category("Agents"),
        );
    }

    for proj in snap
        .projects_list
        .into_iter()
        .take(PALETTE_DYNAMIC_PROJECT_CAP)
    {
        let desc = format!(
            "{} — {} agents, {} msgs",
            proj.human_key, proj.agent_count, proj.message_count
        );
        out.push(
            ActionItem::new(
                format!("{}{}", palette_action_ids::PROJECT_PREFIX, proj.slug),
                format!("Project: {}", proj.slug),
            )
            .with_description(desc)
            .with_tags(&["project"])
            .with_category("Projects"),
        );
    }

    for contact in snap
        .contacts_list
        .into_iter()
        .take(PALETTE_DYNAMIC_CONTACT_CAP)
    {
        let pair = format!("{} → {}", contact.from_agent, contact.to_agent);
        let desc = format!("{} ({})", contact.status, contact.reason);
        out.push(
            ActionItem::new(
                format!(
                    "{}{}:{}",
                    palette_action_ids::CONTACT_PREFIX,
                    contact.from_agent,
                    contact.to_agent
                ),
                format!("Contact: {pair}"),
            )
            .with_description(desc)
            .with_tags(&["contact"])
            .with_category("Contacts"),
        );
    }
}

/// Append palette entries derived from the recent event stream (threads, tools, reservations).
fn build_palette_actions_from_events(state: &TuiSharedState, out: &mut Vec<ActionItem>) {
    let events = state.recent_events(PALETTE_DYNAMIC_EVENT_SCAN);

    let mut threads_seen: HashSet<String> = HashSet::new();
    let mut tools_seen: HashSet<String> = HashSet::new();
    let mut reservations_seen: HashSet<String> = HashSet::new();

    for ev in events.iter().rev() {
        if threads_seen.len() < PALETTE_DYNAMIC_THREAD_CAP {
            if let Some((thread_id, subject)) = extract_thread(ev) {
                if threads_seen.insert(thread_id.to_string()) {
                    out.push(
                        ActionItem::new(
                            format!("{}{}", palette_action_ids::THREAD_PREFIX, thread_id),
                            format!("Thread: {thread_id}"),
                        )
                        .with_description(format!("Latest: {subject}"))
                        .with_tags(&["thread", "messages"])
                        .with_category("Threads"),
                    );
                }
            }
        }

        if tools_seen.len() < PALETTE_DYNAMIC_TOOL_CAP {
            if let Some(tool_name) = extract_tool_name(ev) {
                if tools_seen.insert(tool_name.to_string()) {
                    out.push(
                        ActionItem::new(
                            format!("{}{}", palette_action_ids::TOOL_PREFIX, tool_name),
                            format!("Tool: {tool_name}"),
                        )
                        .with_description("Jump to Tool Metrics screen")
                        .with_tags(&["tool"])
                        .with_category("Tools"),
                    );
                }
            }
        }

        if reservations_seen.len() < PALETTE_DYNAMIC_RESERVATION_CAP {
            if let Some(agent) = extract_reservation_agent(ev) {
                if reservations_seen.insert(agent.to_string()) {
                    out.push(
                        ActionItem::new(
                            format!("{}{}", palette_action_ids::RESERVATION_PREFIX, agent),
                            format!("Reservation: {agent}"),
                        )
                        .with_description("View file reservations for this agent")
                        .with_tags(&["reservation", "file", "lock"])
                        .with_category("Reservations"),
                    );
                }
            }
        }

        if threads_seen.len() >= PALETTE_DYNAMIC_THREAD_CAP
            && tools_seen.len() >= PALETTE_DYNAMIC_TOOL_CAP
            && reservations_seen.len() >= PALETTE_DYNAMIC_RESERVATION_CAP
        {
            break;
        }
    }
}

/// Generate a toast notification for high-priority events.
///
/// Returns `None` for routine events that shouldn't produce toasts.
fn toast_for_event(event: &MailEvent) -> Option<Toast> {
    match event {
        MailEvent::MessageSent { from, to, .. } => {
            let recipients = if to.len() > 2 {
                format!("{} +{}", to[0], to.len() - 1)
            } else {
                to.join(", ")
            };
            Some(
                Toast::new(format!("{from} → {recipients}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(4)),
            )
        }
        MailEvent::AgentRegistered { name, program, .. } => Some(
            Toast::new(format!("{name} ({program})"))
                .icon(ToastIcon::Success)
                .duration(Duration::from_secs(4)),
        ),
        MailEvent::HttpRequest { status, path, .. } if *status >= 500 => Some(
            Toast::new(format!("HTTP {status} on {path}"))
                .icon(ToastIcon::Error)
                .duration(Duration::from_secs(6)),
        ),
        MailEvent::ServerShutdown { .. } => Some(
            Toast::new("Server shutting down")
                .icon(ToastIcon::Warning)
                .duration(Duration::from_secs(8)),
        ),
        MailEvent::ServerStarted { endpoint, .. } => Some(
            Toast::new(format!("Server started at {endpoint}"))
                .icon(ToastIcon::Success)
                .duration(Duration::from_secs(5)),
        ),
        _ => None,
    }
}

fn extract_tool_name(event: &MailEvent) -> Option<&str> {
    match event {
        MailEvent::ToolCallStart { tool_name, .. } | MailEvent::ToolCallEnd { tool_name, .. } => {
            Some(tool_name)
        }
        _ => None,
    }
}

fn extract_thread(event: &MailEvent) -> Option<(&str, &str)> {
    match event {
        MailEvent::MessageSent {
            thread_id, subject, ..
        }
        | MailEvent::MessageReceived {
            thread_id, subject, ..
        } => Some((thread_id, subject)),
        _ => None,
    }
}

fn extract_reservation_agent(event: &MailEvent) -> Option<&str> {
    match event {
        MailEvent::ReservationGranted { agent, .. }
        | MailEvent::ReservationReleased { agent, .. } => Some(agent),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_screens::MailScreenMsg;
    use mcp_agent_mail_core::Config;

    fn test_model() -> MailAppModel {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        MailAppModel::new(state)
    }

    #[test]
    fn initial_screen_is_dashboard() {
        let model = test_model();
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert!(!model.help_visible());
    }

    #[test]
    fn switch_screen_updates_active() {
        let mut model = test_model();
        let cmd = model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert_eq!(model.active_screen(), MailScreenId::Messages);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn toggle_help() {
        let mut model = test_model();
        assert!(!model.help_visible());
        model.update(MailMsg::ToggleHelp);
        assert!(model.help_visible());
        model.update(MailMsg::ToggleHelp);
        assert!(!model.help_visible());
    }

    #[test]
    fn quit_requests_shutdown() {
        let mut model = test_model();
        let cmd = model.update(MailMsg::Quit);
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(cmd, Cmd::Quit));
    }

    #[test]
    fn screen_navigate_switches() {
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::Navigate(
            MailScreenId::Agents,
        )));
        assert_eq!(model.active_screen(), MailScreenId::Agents);
    }

    #[test]
    fn all_screens_have_instances() {
        let model = test_model();
        for &id in ALL_SCREEN_IDS {
            assert!(model.screens.contains_key(&id));
        }
    }

    #[test]
    fn tick_increments_count() {
        let mut model = test_model();
        model.update(MailMsg::Terminal(Event::Tick));
        assert_eq!(model.tick_count, 1);
        model.update(MailMsg::Terminal(Event::Tick));
        assert_eq!(model.tick_count, 2);
    }

    #[test]
    fn map_screen_cmd_preserves_none() {
        assert!(matches!(map_screen_cmd(Cmd::None), Cmd::None));
    }

    #[test]
    fn map_screen_cmd_preserves_quit() {
        assert!(matches!(map_screen_cmd(Cmd::Quit), Cmd::Quit));
    }

    #[test]
    fn map_screen_cmd_wraps_msg() {
        let cmd = map_screen_cmd(Cmd::Msg(MailScreenMsg::Noop));
        assert!(matches!(
            cmd,
            Cmd::Msg(MailMsg::Screen(MailScreenMsg::Noop))
        ));
    }

    #[test]
    fn noop_screen_msg_is_harmless() {
        let mut model = test_model();
        let prev = model.active_screen();
        let cmd = model.update(MailMsg::Screen(MailScreenMsg::Noop));
        assert_eq!(model.active_screen(), prev);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn set_screen_replaces_instance() {
        let mut model = test_model();
        let new_screen = Box::new(AgentsScreen::new());
        model.set_screen(MailScreenId::Agents, new_screen);
        assert!(model.screens.contains_key(&MailScreenId::Agents));
    }

    #[test]
    fn init_returns_batch_with_tick_and_mouse() {
        let mut model = test_model();
        let cmd = model.init();
        // init() now returns Batch([Tick, SetMouseCapture(true)])
        assert!(matches!(cmd, Cmd::Batch(_)));
    }

    #[test]
    fn palette_opens_on_ctrl_p() {
        let mut model = test_model();
        let event = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL),
        );
        model.update(MailMsg::Terminal(event));
        assert!(model.command_palette.is_visible());
    }

    #[test]
    fn palette_dismisses_on_escape() {
        let mut model = test_model();
        let open = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL),
        );
        model.update(MailMsg::Terminal(open));
        assert!(model.command_palette.is_visible());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        model.update(MailMsg::Terminal(esc));
        assert!(!model.command_palette.is_visible());
    }

    #[test]
    fn palette_executes_screen_navigation() {
        let mut model = test_model();
        let open = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL),
        );
        model.update(MailMsg::Terminal(open));

        for ch in "messages".chars() {
            let ev = Event::Key(ftui::KeyEvent::new(KeyCode::Char(ch)));
            model.update(MailMsg::Terminal(ev));
        }

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        model.update(MailMsg::Terminal(enter));
        assert_eq!(model.active_screen(), MailScreenId::Messages);
        assert!(!model.command_palette.is_visible());
    }

    #[test]
    fn deep_link_timeline_switches_to_timeline() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);

        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::TimelineAtTime(50_000_000),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Timeline);
    }

    #[test]
    fn deep_link_message_switches_to_messages() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();

        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::MessageById(42),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Messages);
    }

    #[test]
    fn global_m_key_sends_transport_toggle() {
        use std::sync::mpsc;

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let (tx, rx) = mpsc::channel::<ServerControlMsg>();
        state.set_server_control_sender(tx);

        let mut model = MailAppModel::new(Arc::clone(&state));
        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('m')));
        let _ = model.update(MailMsg::Terminal(event));

        assert_eq!(
            rx.try_recv().ok(),
            Some(ServerControlMsg::ToggleTransportBase)
        );
    }

    // ── Reducer edge-case tests ──────────────────────────────────

    #[test]
    fn tab_cycles_through_all_screens_forward() {
        let mut model = test_model();
        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        let mut visited = vec![model.active_screen()];
        for _ in 0..ALL_SCREEN_IDS.len() {
            model.update(MailMsg::Terminal(tab.clone()));
            visited.push(model.active_screen());
        }
        // After N tabs, should be back to start
        assert_eq!(visited.first(), visited.last());
        // All screens visited
        for &id in ALL_SCREEN_IDS {
            assert!(visited.contains(&id), "screen {id:?} not visited");
        }
    }

    #[test]
    fn backtab_cycles_through_all_screens_backward() {
        let mut model = test_model();
        let backtab = Event::Key(ftui::KeyEvent::new(KeyCode::BackTab));
        let mut visited = vec![model.active_screen()];
        for _ in 0..ALL_SCREEN_IDS.len() {
            model.update(MailMsg::Terminal(backtab.clone()));
            visited.push(model.active_screen());
        }
        assert_eq!(visited.first(), visited.last());
        for &id in ALL_SCREEN_IDS {
            assert!(
                visited.contains(&id),
                "screen {id:?} not visited in reverse"
            );
        }
    }

    #[test]
    fn number_keys_switch_screens() {
        let mut model = test_model();
        // Keys 1..9 map to screens 1..9; key 0 maps to screen 10.
        // Only iterate the first 9 screens with digit keys 1-9.
        for (i, &expected_id) in ALL_SCREEN_IDS.iter().enumerate().take(9) {
            let n = u32::try_from(i + 1).expect("screen index should fit in u32");
            let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char(
                char::from_digit(n, 10).unwrap(),
            )));
            model.update(MailMsg::Terminal(key));
            assert_eq!(
                model.active_screen(),
                expected_id,
                "key {n} -> {expected_id:?}"
            );
        }
        // Key 0 maps to the 10th screen (Contacts).
        if ALL_SCREEN_IDS.len() >= 10 {
            let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('0')));
            model.update(MailMsg::Terminal(key));
            assert_eq!(
                model.active_screen(),
                ALL_SCREEN_IDS[9],
                "key 0 -> screen 10"
            );
        }
    }

    #[test]
    fn number_key_zero_switches_to_projects() {
        let mut model = test_model();
        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('0')));
        model.update(MailMsg::Terminal(key));
        // 0 maps to screen 10 (Projects) — 11th screen (Contacts) needs command palette
        assert_eq!(model.active_screen(), MailScreenId::Projects);
    }

    #[test]
    fn number_key_nine_switches_to_timeline() {
        let mut model = test_model();
        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('9')));
        model.update(MailMsg::Terminal(key));
        assert_eq!(model.active_screen(), MailScreenId::Timeline);
    }

    #[test]
    fn help_and_palette_mutual_exclusivity() {
        let mut model = test_model();

        // Open help
        model.update(MailMsg::ToggleHelp);
        assert!(model.help_visible());

        // Opening palette should close help
        let ctrl_p = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL),
        );
        model.update(MailMsg::Terminal(ctrl_p));
        assert!(!model.help_visible());
        assert!(model.command_palette.is_visible());
    }

    #[test]
    fn escape_closes_help_overlay() {
        let mut model = test_model();
        model.update(MailMsg::ToggleHelp);
        assert!(model.help_visible());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        model.update(MailMsg::Terminal(esc));
        assert!(!model.help_visible());
    }

    #[test]
    fn q_key_triggers_quit() {
        let mut model = test_model();
        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('q')));
        let cmd = model.update(MailMsg::Terminal(key));
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(cmd, Cmd::Quit));
    }

    #[test]
    fn question_mark_toggles_help() {
        let mut model = test_model();
        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('?')));
        model.update(MailMsg::Terminal(key.clone()));
        assert!(model.help_visible());
        model.update(MailMsg::Terminal(key));
        assert!(!model.help_visible());
    }

    #[test]
    fn tick_increments_and_returns_tick_cmd() {
        let mut model = test_model();
        let cmd = model.update(MailMsg::Terminal(Event::Tick));
        assert_eq!(model.tick_count, 1);
        assert!(matches!(cmd, Cmd::Tick(_)));
    }

    #[test]
    fn deep_link_thread_by_id_switches_to_threads() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::ThreadById("br-10wc".to_string()),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Threads);
    }

    #[test]
    fn deep_link_agent_switches_to_agents() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::AgentByName("RedFox".to_string()),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Agents);
    }

    #[test]
    fn deep_link_tool_switches_to_tool_metrics() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::ToolByName("send_message".to_string()),
        )));
        assert_eq!(model.active_screen(), MailScreenId::ToolMetrics);
    }

    #[test]
    fn deep_link_project_switches_to_projects() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::ProjectBySlug("my-proj".to_string()),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Projects);
    }

    #[test]
    fn deep_link_reservation_switches_to_reservations() {
        use crate::tui_screens::DeepLinkTarget;
        let mut model = test_model();
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
            DeepLinkTarget::ReservationByAgent("BlueLake".to_string()),
        )));
        assert_eq!(model.active_screen(), MailScreenId::Reservations);
    }

    #[test]
    fn screen_navigation_msg_and_switch_screen_are_equivalent() {
        let mut model1 = test_model();
        let mut model2 = test_model();

        model1.update(MailMsg::Screen(MailScreenMsg::Navigate(
            MailScreenId::Agents,
        )));
        model2.update(MailMsg::SwitchScreen(MailScreenId::Agents));

        assert_eq!(model1.active_screen(), model2.active_screen());
    }

    #[test]
    fn colon_opens_palette() {
        let mut model = test_model();
        let colon = Event::Key(ftui::KeyEvent::new(KeyCode::Char(':')));
        model.update(MailMsg::Terminal(colon));
        assert!(model.command_palette.is_visible());
    }

    #[test]
    fn palette_blocks_global_shortcuts() {
        let mut model = test_model();

        // Open palette
        let ctrl_p = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL),
        );
        model.update(MailMsg::Terminal(ctrl_p));
        assert!(model.command_palette.is_visible());

        // 'q' while palette is open should NOT quit
        let q = Event::Key(ftui::KeyEvent::new(KeyCode::Char('q')));
        let cmd = model.update(MailMsg::Terminal(q));
        assert!(!model.state.is_shutdown_requested());
        assert!(!matches!(cmd, Cmd::Quit));
    }

    #[test]
    fn with_config_preserves_state() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(Arc::clone(&state), &config);
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        assert!(!model.help_visible());
        // Should have all screens
        for &id in ALL_SCREEN_IDS {
            assert!(model.screens.contains_key(&id));
        }
    }

    #[test]
    fn palette_action_ids_cover_all_screens() {
        for &id in ALL_SCREEN_IDS {
            let action_id = screen_palette_action_id(id);
            let round_tripped = screen_from_palette_action_id(action_id);
            assert_eq!(round_tripped, Some(id), "round-trip failed for {id:?}");
        }
    }

    #[test]
    fn palette_action_ids_unknown_returns_none() {
        assert_eq!(screen_from_palette_action_id("screen:unknown"), None);
        assert_eq!(screen_from_palette_action_id(""), None);
    }

    #[test]
    fn build_palette_actions_static_has_screens_and_app_controls() {
        let actions = build_palette_actions_static();
        // Should have one action per screen + transport actions + app controls
        assert!(actions.len() >= ALL_SCREEN_IDS.len() + 2);
        // Check that screen actions are present
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        for &screen_id in ALL_SCREEN_IDS {
            let action_id = screen_palette_action_id(screen_id);
            assert!(
                ids.contains(&action_id),
                "missing palette action for {screen_id:?}"
            );
        }
        assert!(ids.contains(&palette_action_ids::APP_QUIT));
        assert!(ids.contains(&palette_action_ids::APP_TOGGLE_HELP));
    }

    #[test]
    fn map_screen_cmd_maps_all_variants() {
        // Tick
        let cmd = map_screen_cmd(Cmd::Tick(std::time::Duration::from_millis(100)));
        assert!(matches!(cmd, Cmd::Tick(_)));

        // Log
        let cmd = map_screen_cmd(Cmd::Log("test".into()));
        assert!(matches!(cmd, Cmd::Log(_)));

        // Batch
        let cmd = map_screen_cmd(Cmd::Batch(vec![Cmd::None, Cmd::Quit]));
        assert!(matches!(cmd, Cmd::Batch(_)));

        // Sequence (must have 2+ elements; single-element collapses)
        let cmd = map_screen_cmd(Cmd::Sequence(vec![Cmd::None, Cmd::Quit]));
        assert!(matches!(cmd, Cmd::Sequence(_) | Cmd::Batch(_)));

        // SaveState / RestoreState
        assert!(matches!(map_screen_cmd(Cmd::SaveState), Cmd::SaveState));
        assert!(matches!(
            map_screen_cmd(Cmd::RestoreState),
            Cmd::RestoreState
        ));

        // SetMouseCapture
        assert!(matches!(
            map_screen_cmd(Cmd::SetMouseCapture(true)),
            Cmd::SetMouseCapture(true)
        ));
    }

    #[test]
    fn dispatch_palette_help_toggles_help() {
        let mut model = test_model();
        assert!(!model.help_visible());
        model.dispatch_palette_action(palette_action_ids::APP_TOGGLE_HELP);
        assert!(model.help_visible());
    }

    #[test]
    fn dispatch_palette_quit_requests_shutdown() {
        let mut model = test_model();
        let cmd = model.dispatch_palette_action(palette_action_ids::APP_QUIT);
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(cmd, Cmd::Quit));
    }

    #[test]
    fn dispatch_palette_screen_navigation() {
        let mut model = test_model();
        model.dispatch_palette_action(palette_action_ids::SCREEN_MESSAGES);
        assert_eq!(model.active_screen(), MailScreenId::Messages);
    }

    #[test]
    fn dispatch_palette_agent_prefix_goes_to_agents() {
        let mut model = test_model();
        model.dispatch_palette_action("agent:GoldFox");
        assert_eq!(model.active_screen(), MailScreenId::Agents);
    }

    #[test]
    fn dispatch_palette_thread_prefix_goes_to_threads() {
        let mut model = test_model();
        model.dispatch_palette_action("thread:br-10wc");
        assert_eq!(model.active_screen(), MailScreenId::Threads);
    }

    #[test]
    fn dispatch_palette_tool_prefix_goes_to_tool_metrics() {
        let mut model = test_model();
        model.dispatch_palette_action("tool:fetch_inbox");
        assert_eq!(model.active_screen(), MailScreenId::ToolMetrics);
    }

    #[test]
    fn dispatch_palette_unknown_id_is_noop() {
        let mut model = test_model();
        let prev = model.active_screen();
        let cmd = model.dispatch_palette_action("unknown:foo");
        assert_eq!(model.active_screen(), prev);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn dispatch_palette_layout_reset_returns_none() {
        let mut model = test_model();
        let cmd = model.dispatch_palette_action(palette_action_ids::LAYOUT_RESET);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn dispatch_palette_layout_export_returns_none() {
        let mut model = test_model();
        let cmd = model.dispatch_palette_action(palette_action_ids::LAYOUT_EXPORT);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn dispatch_palette_layout_import_returns_none() {
        let mut model = test_model();
        let cmd = model.dispatch_palette_action(palette_action_ids::LAYOUT_IMPORT);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn palette_static_actions_include_layout_controls() {
        let actions = build_palette_actions_static();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&palette_action_ids::LAYOUT_RESET));
        assert!(ids.contains(&palette_action_ids::LAYOUT_EXPORT));
        assert!(ids.contains(&palette_action_ids::LAYOUT_IMPORT));
    }

    // ── Accessibility tests ─────────────────────────────────────

    #[test]
    fn default_accessibility_settings() {
        let model = test_model();
        assert!(!model.accessibility().high_contrast);
        assert!(model.accessibility().key_hints);
    }

    #[test]
    fn toggle_high_contrast_via_palette() {
        let mut model = test_model();
        assert!(!model.accessibility().high_contrast);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HC);
        assert!(model.accessibility().high_contrast);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HC);
        assert!(!model.accessibility().high_contrast);
    }

    #[test]
    fn toggle_key_hints_via_palette() {
        let mut model = test_model();
        assert!(model.accessibility().key_hints);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HINTS);
        assert!(!model.accessibility().key_hints);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HINTS);
        assert!(model.accessibility().key_hints);
    }

    #[test]
    fn palette_static_actions_include_accessibility_controls() {
        let actions = build_palette_actions_static();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_HC));
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_HINTS));
    }

    #[test]
    fn with_config_loads_accessibility_settings() {
        let config = mcp_agent_mail_core::Config {
            tui_high_contrast: true,
            tui_key_hints: false,
            ..mcp_agent_mail_core::Config::default()
        };
        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(Arc::clone(&state), &config);
        assert!(model.accessibility().high_contrast);
        assert!(!model.accessibility().key_hints);
    }

    // ── Quick action dispatch tests ─────────────────────────────

    #[test]
    fn dispatch_quick_agent_navigates_to_agents() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:agent:RedFox");
        assert_eq!(model.active_screen(), MailScreenId::Agents);
    }

    #[test]
    fn dispatch_quick_thread_navigates_to_threads() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:thread:abc123");
        assert_eq!(model.active_screen(), MailScreenId::Threads);
    }

    #[test]
    fn dispatch_quick_tool_navigates_to_tool_metrics() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:tool:send_message");
        assert_eq!(model.active_screen(), MailScreenId::ToolMetrics);
    }

    #[test]
    fn dispatch_quick_message_navigates_to_messages() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:message:42");
        assert_eq!(model.active_screen(), MailScreenId::Messages);
    }

    #[test]
    fn dispatch_quick_project_navigates_to_projects() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:project:my_proj");
        assert_eq!(model.active_screen(), MailScreenId::Projects);
    }

    #[test]
    fn dispatch_unknown_quick_action_is_noop() {
        let mut model = test_model();
        model.dispatch_palette_action("quick:unknown:foo");
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
    }

    #[test]
    fn dispatch_palette_project_prefix_goes_to_projects() {
        let mut model = test_model();
        model.dispatch_palette_action("project:my-proj");
        assert_eq!(model.active_screen(), MailScreenId::Projects);
    }

    #[test]
    fn dispatch_palette_contact_prefix_goes_to_contacts() {
        let mut model = test_model();
        model.dispatch_palette_action("contact:BlueLake:RedFox");
        assert_eq!(model.active_screen(), MailScreenId::Contacts);
    }

    #[test]
    fn dispatch_palette_contact_no_colon_goes_to_contacts() {
        let mut model = test_model();
        model.dispatch_palette_action("contact:malformed");
        assert_eq!(model.active_screen(), MailScreenId::Contacts);
    }

    #[test]
    fn dispatch_palette_reservation_prefix_goes_to_reservations() {
        let mut model = test_model();
        model.dispatch_palette_action("reservation:BlueLake");
        assert_eq!(model.active_screen(), MailScreenId::Reservations);
    }

    #[test]
    fn extract_reservation_agent_from_events() {
        let ev1 = MailEvent::reservation_granted("TestAgent", vec![], true, 60, "proj");
        let ev2 = MailEvent::reservation_released("OtherAgent", vec![], "proj");
        let ev3 =
            MailEvent::tool_call_start("foo", serde_json::json!({}), Some("proj".into()), None);
        assert_eq!(extract_reservation_agent(&ev1), Some("TestAgent"));
        assert_eq!(extract_reservation_agent(&ev2), Some("OtherAgent"));
        assert_eq!(extract_reservation_agent(&ev3), None);
    }
}
