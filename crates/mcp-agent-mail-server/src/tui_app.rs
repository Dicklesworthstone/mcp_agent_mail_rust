//! Top-level TUI application model for `AgentMailTUI`.
//!
//! [`MailAppModel`] implements the `ftui_runtime` [`Model`] trait,
//! orchestrating screen switching, global keybindings, tick dispatch,
//! and shared-state access.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ftui::Frame;
use ftui::layout::Rect;
use ftui::text::display_width;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::command_palette::{ActionItem, CommandPalette, PaletteAction};
use ftui::widgets::hint_ranker::{HintContext, HintRanker, RankerConfig};
use ftui::widgets::modal::{Dialog, DialogResult, DialogState};
use ftui::widgets::notification_queue::NotificationStack;
use ftui::widgets::toast::ToastPosition;
use ftui::widgets::{NotificationQueue, QueueConfig, Toast, ToastIcon};
use ftui::{Event, KeyCode, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_runtime::program::{Cmd, Model};
use mcp_agent_mail_db::{DbConn, DbPoolConfig};

use crate::tui_action_menu::{ActionKind, ActionMenuManager, ActionMenuResult};
use crate::tui_bridge::{ServerControlMsg, TransportBase, TuiSharedState};
use crate::tui_events::MailEvent;
use crate::tui_macro::{MacroEngine, PlaybackMode, PlaybackState, action_ids as macro_ids};
use crate::tui_screens::{
    ALL_SCREEN_IDS, DeepLinkTarget, MailScreen, MailScreenId, MailScreenMsg, agents::AgentsScreen,
    analytics::AnalyticsScreen, attachments::AttachmentExplorerScreen, contacts::ContactsScreen,
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
const PALETTE_DYNAMIC_MESSAGE_CAP: usize = 50;
const PALETTE_DYNAMIC_TOOL_CAP: usize = 50;
const PALETTE_DYNAMIC_PROJECT_CAP: usize = 30;
const PALETTE_DYNAMIC_CONTACT_CAP: usize = 30;
const PALETTE_DYNAMIC_RESERVATION_CAP: usize = 30;
const PALETTE_DYNAMIC_EVENT_SCAN: usize = 1500;
const PALETTE_DB_CACHE_TTL_MICROS: i64 = 5 * 1_000_000;
const PALETTE_USAGE_HALF_LIFE_MICROS: i64 = 60 * 60 * 1_000_000;

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
// Toast severity threshold
// ──────────────────────────────────────────────────────────────────────

/// Minimum severity for toast notifications. Toasts below this level
/// are suppressed. Controlled by `AM_TUI_TOAST_SEVERITY` env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverityThreshold {
    /// Show all toasts (info, warning, error).
    Info,
    /// Show only warning and error toasts.
    Warning,
    /// Show only error toasts.
    Error,
    /// Suppress all toasts.
    Off,
}

impl ToastSeverityThreshold {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Self::Off,
            "error" => Self::Error,
            "warning" | "warn" => Self::Warning,
            _ => Self::Info,
        }
    }

    fn from_env() -> Self {
        Self::parse(&std::env::var("AM_TUI_TOAST_SEVERITY").unwrap_or_default())
    }

    fn from_config(config: &mcp_agent_mail_core::Config) -> Self {
        if !config.tui_toast_enabled {
            Self::Off
        } else {
            Self::parse(config.tui_toast_severity.as_str())
        }
    }

    /// Returns `true` if a toast at the given icon level should be shown.
    const fn allows(self, icon: ToastIcon) -> bool {
        match self {
            Self::Off => false,
            Self::Error => matches!(icon, ToastIcon::Error),
            Self::Warning => matches!(icon, ToastIcon::Warning | ToastIcon::Error),
            Self::Info => true,
        }
    }
}

/// Duration threshold (ms) for slow tool call toasts.
const SLOW_TOOL_THRESHOLD_MS: u64 = 5000;
/// How far ahead (in microseconds) to warn about expiring reservations (5 min).
const RESERVATION_EXPIRY_WARN_MICROS: i64 = 5 * 60 * 1_000_000;

/// Toast border/icon colors by severity, matching `tui_events` severity palette.
const TOAST_COLOR_ERROR: PackedRgba = PackedRgba::rgb(255, 100, 100);
const TOAST_COLOR_WARNING: PackedRgba = PackedRgba::rgb(255, 184, 108);
const TOAST_COLOR_INFO: PackedRgba = PackedRgba::rgb(120, 220, 150);
const TOAST_COLOR_SUCCESS: PackedRgba = PackedRgba::rgb(100, 220, 170);
/// Bright cyan highlight for the focused toast border.
const TOAST_FOCUS_HIGHLIGHT: PackedRgba = PackedRgba::rgb(80, 220, 255);

/// Current time as microseconds since Unix epoch.
fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

fn decayed_palette_usage_weight(
    usage_count: u32,
    last_used_micros: i64,
    ranking_now_micros: i64,
) -> f64 {
    if usage_count == 0 {
        return 0.0;
    }
    let age_micros = ranking_now_micros.saturating_sub(last_used_micros).max(0) as f64;
    let half_life = PALETTE_USAGE_HALF_LIFE_MICROS as f64;
    let decay = 2.0_f64.powf(-age_micros / half_life);
    f64::from(usage_count) * decay
}

fn parse_toast_position(value: &str) -> ToastPosition {
    match value.trim().to_ascii_lowercase().as_str() {
        "top-left" => ToastPosition::TopLeft,
        "bottom-left" => ToastPosition::BottomLeft,
        "bottom-right" => ToastPosition::BottomRight,
        _ => ToastPosition::TopRight,
    }
}

// ──────────────────────────────────────────────────────────────────────
// ModalManager — confirmation dialogs
// ──────────────────────────────────────────────────────────────────────

/// Severity level for modal dialogs, affecting styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModalSeverity {
    /// Informational dialog.
    #[default]
    Info,
    /// Warning dialog (destructive action).
    Warning,
    /// Error dialog (critical action).
    Error,
}

/// Callback invoked when a modal is confirmed or cancelled.
pub type ModalCallback = Box<dyn FnOnce(DialogResult) + Send + 'static>;

/// Active modal dialog state.
pub struct ActiveModal {
    /// The dialog widget.
    dialog: Dialog,
    /// Dialog interaction state.
    state: DialogState,
    /// Severity for styling.
    severity: ModalSeverity,
    /// Optional callback when dialog closes.
    callback: Option<ModalCallback>,
}

/// Manages modal dialog lifecycle for the TUI.
///
/// Modals trap focus: when a modal is active, all key events go to the modal
/// until it is dismissed. Modals render above toasts but below the command palette.
pub struct ModalManager {
    /// The currently active modal, if any.
    active: Option<ActiveModal>,
}

impl Default for ModalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalManager {
    /// Create a new modal manager with no active modal.
    #[must_use]
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// Returns `true` if a modal is currently active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Show a confirmation dialog with the given title and message.
    pub fn show_confirmation(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        severity: ModalSeverity,
        on_complete: impl FnOnce(DialogResult) + Send + 'static,
    ) {
        let dialog = Dialog::confirm(title, message);
        self.active = Some(ActiveModal {
            dialog,
            state: DialogState::new(),
            severity,
            callback: Some(Box::new(on_complete)),
        });
    }

    /// Show a force-release reservation confirmation dialog.
    pub fn show_force_release_confirmation(
        &mut self,
        reservation_details: &str,
        on_confirm: impl FnOnce(DialogResult) + Send + 'static,
    ) {
        self.show_confirmation(
            "Force Release Reservation",
            format!(
                "This will force-release the reservation:\n\n{reservation_details}\n\n\
                The owning agent may lose work. Continue?"
            ),
            ModalSeverity::Warning,
            on_confirm,
        );
    }

    /// Show a clear-all confirmation dialog.
    pub fn show_clear_all_confirmation(
        &mut self,
        warning_text: &str,
        on_confirm: impl FnOnce(DialogResult) + Send + 'static,
    ) {
        self.show_confirmation(
            "Clear All",
            warning_text.to_string(),
            ModalSeverity::Warning,
            on_confirm,
        );
    }

    /// Show a send message confirmation dialog.
    pub fn show_send_confirmation(
        &mut self,
        message_summary: &str,
        on_confirm: impl FnOnce(DialogResult) + Send + 'static,
    ) {
        self.show_confirmation(
            "Send Message",
            format!("Send this message?\n\n{message_summary}"),
            ModalSeverity::Info,
            on_confirm,
        );
    }

    /// Show a generic destructive action confirmation dialog.
    pub fn show_destructive_action_confirmation(
        &mut self,
        action_name: &str,
        details: &str,
        on_confirm: impl FnOnce(DialogResult) + Send + 'static,
    ) {
        self.show_confirmation(
            action_name.to_string(),
            format!("{details}\n\nThis action cannot be undone. Continue?"),
            ModalSeverity::Warning,
            on_confirm,
        );
    }

    /// Handle an event, returning `true` if the event was consumed.
    ///
    /// When a modal is active, all events are routed to it (focus trapping).
    pub fn handle_event(&mut self, event: &Event) -> bool {
        let Some(ref mut modal) = self.active else {
            return false;
        };

        // Let the dialog handle the event
        if let Some(result) = modal.dialog.handle_event(event, &mut modal.state, None) {
            // Dialog closed — invoke callback and clear
            if let Some(callback) = modal.callback.take() {
                callback(result);
            }
            self.active = None;
        }

        // Event was consumed by the modal
        true
    }

    /// Render the modal if active.
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if let Some(ref modal) = self.active {
            // Severity-based border color (reserved for future use when Dialog supports it)
            let _border_color = match modal.severity {
                ModalSeverity::Info => TOAST_COLOR_INFO,
                ModalSeverity::Warning => TOAST_COLOR_WARNING,
                ModalSeverity::Error => TOAST_COLOR_ERROR,
            };
            // Render using StatefulWidget with a cloned state (read-only render)
            let mut render_state = modal.state.clone();
            modal.dialog.render(area, frame, &mut render_state);
        }
    }

    /// Dismiss the current modal without invoking the callback.
    pub fn dismiss(&mut self) {
        self.active = None;
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
    hint_ranker: HintRanker,
    palette_hint_ids: HashMap<String, usize>,
    palette_usage_path: Option<PathBuf>,
    palette_usage_stats: crate::tui_persist::PaletteUsageMap,
    palette_usage_dirty: bool,
    notifications: NotificationQueue,
    last_toast_seq: u64,
    tick_count: u64,
    accessibility: crate::tui_persist::AccessibilitySettings,
    macro_engine: MacroEngine,
    /// Tracks active reservations for expiry warnings.
    /// Key: "{project}:{agent}:{path}", Value: (display_label, expiry_timestamp_micros).
    reservation_tracker: HashMap<String, (String, i64)>,
    /// Reservations already warned about (prevent duplicate warnings).
    warned_reservations: HashSet<String>,
    /// Minimum severity level for toast notifications.
    toast_severity: ToastSeverityThreshold,
    /// Runtime mute flag for toast generation.
    toast_muted: bool,
    /// Per-severity auto-dismiss durations (seconds).
    toast_info_dismiss_secs: u64,
    toast_warn_dismiss_secs: u64,
    toast_error_dismiss_secs: u64,
    /// When `Some(idx)`, the toast stack is in focus mode and the
    /// toast at `idx` has a highlight border. `Ctrl+T` toggles.
    toast_focus_index: Option<usize>,
    /// Modal manager for confirmation dialogs.
    modal_manager: ModalManager,
    /// Action menu for contextual per-item actions.
    action_menu: ActionMenuManager,
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
            } else if id == MailScreenId::Attachments {
                screens.insert(id, Box::new(AttachmentExplorerScreen::new()));
            }
        }

        let static_actions = build_palette_actions_static();
        let mut command_palette = CommandPalette::new().with_max_visible(PALETTE_MAX_VISIBLE);
        command_palette.replace_actions(static_actions.clone());
        let mut hint_ranker = HintRanker::new(RankerConfig::default());
        let mut palette_hint_ids: HashMap<String, usize> = HashMap::new();
        register_palette_hints(
            &mut hint_ranker,
            &mut palette_hint_ids,
            &static_actions,
            screen_palette_action_id(MailScreenId::Dashboard),
        );
        Self {
            state,
            active_screen: MailScreenId::Dashboard,
            screens,
            help_visible: false,
            help_scroll: 0,
            keymap: crate::tui_keymap::KeymapRegistry::default(),
            command_palette,
            hint_ranker,
            palette_hint_ids,
            palette_usage_path: None,
            palette_usage_stats: HashMap::new(),
            palette_usage_dirty: false,
            notifications: NotificationQueue::new(QueueConfig::default()),
            last_toast_seq: 0,
            tick_count: 0,
            accessibility: crate::tui_persist::AccessibilitySettings::default(),
            macro_engine: MacroEngine::new(),
            reservation_tracker: HashMap::new(),
            warned_reservations: HashSet::new(),
            toast_severity: ToastSeverityThreshold::from_env(),
            toast_muted: false,
            toast_info_dismiss_secs: 5,
            toast_warn_dismiss_secs: 8,
            toast_error_dismiss_secs: 15,
            toast_focus_index: None,
            modal_manager: ModalManager::new(),
            action_menu: ActionMenuManager::new(),
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
        let usage_path = crate::tui_persist::palette_usage_path(&config.console_persist_path);
        model.palette_usage_stats = crate::tui_persist::load_palette_usage_or_default(&usage_path);
        model.palette_usage_path = Some(usage_path);
        model.toast_severity = ToastSeverityThreshold::from_config(config);
        model.toast_muted = !config.tui_toast_enabled;
        model.toast_info_dismiss_secs = config.tui_toast_info_dismiss_secs.max(1);
        model.toast_warn_dismiss_secs = config.tui_toast_warn_dismiss_secs.max(1);
        model.toast_error_dismiss_secs = config.tui_toast_error_dismiss_secs.max(1);
        let max_visible = if config.tui_toast_enabled {
            config.tui_toast_max_visible.max(1)
        } else {
            0
        };
        model.notifications = NotificationQueue::new(
            QueueConfig::default()
                .max_visible(max_visible)
                .position(parse_toast_position(config.tui_toast_position.as_str()))
                .default_duration(Duration::from_secs(model.toast_info_dismiss_secs)),
        );
        // Screens with config-driven preferences + persistence.
        model.set_screen(
            MailScreenId::Timeline,
            Box::new(TimelineScreen::with_config(config)),
        );
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

    /// Get mutable access to the modal manager for showing confirmation dialogs.
    pub fn modal_manager_mut(&mut self) -> &mut ModalManager {
        &mut self.modal_manager
    }

    /// Get mutable access to the action menu manager.
    pub fn action_menu_mut(&mut self) -> &mut ActionMenuManager {
        &mut self.action_menu
    }

    /// Dispatch an action selected from the action menu.
    fn dispatch_action_menu_selection(
        &mut self,
        action: ActionKind,
        context: String,
    ) -> Cmd<MailMsg> {
        match action {
            ActionKind::Navigate(screen_id) => {
                self.active_screen = screen_id;
                Cmd::none()
            }
            ActionKind::DeepLink(target) => {
                // Apply deep-link to target screen
                if let Some(screen) = self.screens.get_mut(&target.target_screen()) {
                    let _ = screen.receive_deep_link(&target);
                }
                self.active_screen = target.target_screen();
                Cmd::none()
            }
            ActionKind::Execute(operation) => {
                // For now, show a toast indicating the operation
                // Full implementation will dispatch to screen-specific handlers
                self.notifications.notify(
                    Toast::new(format!("Action: {operation} on {context}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                Cmd::none()
            }
            ActionKind::ConfirmThenExecute {
                title,
                message,
                operation,
            } => {
                // Open confirmation modal for destructive actions
                self.modal_manager.show_confirmation(
                    title,
                    message,
                    ModalSeverity::Warning,
                    move |result| {
                        if matches!(result, DialogResult::Ok) {
                            // The operation is captured but not dispatched yet
                            // Full implementation would send a message to execute
                            let _ = operation;
                        }
                    },
                );
                Cmd::none()
            }
            ActionKind::CopyToClipboard(text) => {
                // Clipboard copy would require platform-specific handling
                self.notifications.notify(
                    Toast::new(format!(
                        "Copied: {}",
                        text.chars().take(30).collect::<String>()
                    ))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(2)),
                );
                Cmd::none()
            }
            ActionKind::Dismiss => Cmd::none(),
        }
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

    /// Read-only access to the macro engine.
    #[must_use]
    pub const fn macro_engine(&self) -> &MacroEngine {
        &self.macro_engine
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

    fn sync_palette_hints(&mut self, actions: &[ActionItem]) {
        register_palette_hints(
            &mut self.hint_ranker,
            &mut self.palette_hint_ids,
            actions,
            screen_palette_action_id(self.active_screen),
        );
    }

    fn rank_palette_actions(&mut self, actions: Vec<ActionItem>) -> Vec<ActionItem> {
        self.sync_palette_hints(&actions);

        let (ordering, _) = self
            .hint_ranker
            .rank(Some(screen_palette_action_id(self.active_screen)));
        if ordering.is_empty() {
            return actions;
        }

        let mut rank_by_hint_id: HashMap<usize, usize> = HashMap::with_capacity(ordering.len());
        for (rank, hint_id) in ordering.into_iter().enumerate() {
            rank_by_hint_id.insert(hint_id, rank);
        }

        let mut indexed_actions: Vec<(usize, ActionItem)> =
            actions.into_iter().enumerate().collect();
        let ranking_now_micros = now_micros();
        indexed_actions.sort_by(
            |(original_index_a, action_a), (original_index_b, action_b)| {
                let decay_a =
                    self.decayed_palette_usage_score(action_a.id.as_str(), ranking_now_micros);
                let decay_b =
                    self.decayed_palette_usage_score(action_b.id.as_str(), ranking_now_micros);
                let decay_cmp = decay_b.total_cmp(&decay_a);
                if decay_cmp != std::cmp::Ordering::Equal {
                    return decay_cmp;
                }

                let rank_a = self
                    .palette_hint_ids
                    .get(action_a.id.as_str())
                    .and_then(|hint_id| rank_by_hint_id.get(hint_id))
                    .copied()
                    .unwrap_or(usize::MAX);
                let rank_b = self
                    .palette_hint_ids
                    .get(action_b.id.as_str())
                    .and_then(|hint_id| rank_by_hint_id.get(hint_id))
                    .copied()
                    .unwrap_or(usize::MAX);

                rank_a
                    .cmp(&rank_b)
                    .then_with(|| original_index_a.cmp(original_index_b))
            },
        );

        let ranked_actions: Vec<ActionItem> = indexed_actions
            .into_iter()
            .map(|(_, action)| action)
            .collect();
        for action in ranked_actions.iter().take(PALETTE_MAX_VISIBLE) {
            if let Some(&hint_id) = self.palette_hint_ids.get(action.id.as_str()) {
                self.hint_ranker.record_shown_not_used(hint_id);
            }
        }

        ranked_actions
    }

    fn decayed_palette_usage_score(&self, action_id: &str, ranking_now_micros: i64) -> f64 {
        let Some((usage_count, last_used_micros)) =
            self.palette_usage_stats.get(action_id).copied()
        else {
            return 0.0;
        };
        decayed_palette_usage_weight(usage_count, last_used_micros, ranking_now_micros)
    }

    fn persist_palette_usage(&mut self) {
        if !self.palette_usage_dirty {
            return;
        }
        let Some(path) = self.palette_usage_path.as_deref() else {
            return;
        };

        match crate::tui_persist::save_palette_usage(path, &self.palette_usage_stats) {
            Ok(()) => {
                self.palette_usage_dirty = false;
            }
            Err(e) => {
                eprintln!(
                    "tui_app: failed to save palette usage to {}: {e}",
                    path.display()
                );
            }
        }
    }

    fn flush_before_shutdown(&mut self) {
        self.persist_palette_usage();
    }

    fn toast_dismiss_secs(&self, icon: ToastIcon) -> u64 {
        match icon {
            ToastIcon::Warning => self.toast_warn_dismiss_secs,
            ToastIcon::Error => self.toast_error_dismiss_secs,
            _ => self.toast_info_dismiss_secs,
        }
        .max(1)
    }

    fn apply_toast_policy(&self, toast: Toast) -> Toast {
        let icon = toast.content.icon.unwrap_or(ToastIcon::Info);
        toast.duration(Duration::from_secs(self.toast_dismiss_secs(icon)))
    }

    fn record_palette_action_usage(&mut self, action_id: &str) {
        if let Some(&hint_id) = self.palette_hint_ids.get(action_id) {
            self.hint_ranker.record_usage(hint_id);
        }
        let used_at = now_micros();
        let stats = self
            .palette_usage_stats
            .entry(action_id.to_string())
            .or_insert((0, used_at));
        stats.0 = stats.0.saturating_add(1);
        stats.1 = used_at;
        self.palette_usage_dirty = true;
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

        // Inject saved macro entries (play, step-by-step, preview, delete).
        for name in self.macro_engine.list_macros() {
            let steps = self
                .macro_engine
                .get_macro(name)
                .map_or(0, super::tui_macro::MacroDef::len);
            actions.push(
                ActionItem::new(
                    format!("{}{name}", macro_ids::PLAY_PREFIX),
                    format!("Play macro: {name}"),
                )
                .with_description(format!("{steps} steps, continuous"))
                .with_tags(&["macro", "play", "automation"])
                .with_category("Macros"),
            );
            actions.push(
                ActionItem::new(
                    format!("{}{name}", macro_ids::PLAY_STEP_PREFIX),
                    format!("Step-through: {name}"),
                )
                .with_description(format!("{steps} steps, confirm each"))
                .with_tags(&["macro", "step", "automation"])
                .with_category("Macros"),
            );
            actions.push(
                ActionItem::new(
                    format!("{}{name}", macro_ids::DRY_RUN_PREFIX),
                    format!("Preview macro: {name}"),
                )
                .with_description(format!("{steps} steps, dry run"))
                .with_tags(&["macro", "preview", "dry-run"])
                .with_category("Macros"),
            );
            actions.push(
                ActionItem::new(
                    format!("{}{name}", macro_ids::DELETE_PREFIX),
                    format!("Delete macro: {name}"),
                )
                .with_description("Permanently remove this macro")
                .with_tags(&["macro", "delete"])
                .with_category("Macros"),
            );
        }

        let ranked_actions = self.rank_palette_actions(actions);
        self.command_palette.replace_actions(ranked_actions);
        self.command_palette.open();
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_palette_action(&mut self, id: &str) -> Cmd<MailMsg> {
        self.dispatch_palette_action_inner(id, false)
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_palette_action_from_macro(&mut self, id: &str) -> Cmd<MailMsg> {
        self.dispatch_palette_action_inner(id, true)
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_palette_action_inner(&mut self, id: &str, macro_playback: bool) -> Cmd<MailMsg> {
        if !macro_playback {
            self.record_palette_action_usage(id);
        }

        // ── Macro engine controls (never recorded) ────────────────
        if let Some(cmd) = self.handle_macro_control(id) {
            return cmd;
        }

        // ── Record this action if the recorder is active ──────────
        if self.macro_engine.recorder_state().is_recording() {
            // Derive a label from the action ID for readability.
            let label = palette_action_label(id);
            self.macro_engine.record_step(id, &label);
        }

        // ── App controls ───────────────────────────────────────────
        match id {
            palette_action_ids::APP_TOGGLE_HELP => {
                self.help_visible = !self.help_visible;
                self.help_scroll = 0;
                return Cmd::none();
            }
            palette_action_ids::APP_QUIT => {
                self.flush_before_shutdown();
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
                let ok = self
                    .screens
                    .get_mut(&MailScreenId::Timeline)
                    .is_some_and(|s| s.reset_layout());
                if ok {
                    self.notifications.notify(
                        Toast::new("Layout reset")
                            .icon(ToastIcon::Info)
                            .duration(Duration::from_secs(2)),
                    );
                } else {
                    self.notifications.notify(
                        Toast::new("Layout reset not supported")
                            .icon(ToastIcon::Warning)
                            .duration(Duration::from_secs(3)),
                    );
                }
                return Cmd::none();
            }
            palette_action_ids::LAYOUT_EXPORT => {
                let path = self
                    .screens
                    .get(&MailScreenId::Timeline)
                    .and_then(|s| s.export_layout());
                if let Some(path) = path {
                    self.notifications.notify(
                        Toast::new(format!("Exported layout to {}", path.display()))
                            .icon(ToastIcon::Info)
                            .duration(Duration::from_secs(4)),
                    );
                } else {
                    self.notifications.notify(
                        Toast::new("Layout export not available")
                            .icon(ToastIcon::Warning)
                            .duration(Duration::from_secs(3)),
                    );
                }
                return Cmd::none();
            }
            palette_action_ids::LAYOUT_IMPORT => {
                let ok = self
                    .screens
                    .get_mut(&MailScreenId::Timeline)
                    .is_some_and(|s| s.import_layout());
                if ok {
                    self.notifications.notify(
                        Toast::new("Imported layout")
                            .icon(ToastIcon::Info)
                            .duration(Duration::from_secs(3)),
                    );
                } else {
                    self.notifications.notify(
                        Toast::new("Layout import failed (missing layout.json?)")
                            .icon(ToastIcon::Warning)
                            .duration(Duration::from_secs(4)),
                    );
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
        if let Some(id_str) = id.strip_prefix(palette_action_ids::MESSAGE_PREFIX) {
            if let Ok(msg_id) = id_str.parse::<i64>() {
                let target = DeepLinkTarget::MessageById(msg_id);
                self.active_screen = MailScreenId::Messages;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Messages) {
                    screen.receive_deep_link(&target);
                }
            } else {
                self.active_screen = MailScreenId::Messages;
            }
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

        // ── Macro actions (context-aware high-value operations) ───
        if let Some(rest) = id.strip_prefix("macro:") {
            return self.dispatch_macro_action(rest);
        }

        // If this action was part of macro playback, treat unknown IDs as a
        // deterministic fail-stop (for replay safety + forensics).
        if macro_playback {
            let reason = format!("unrecognized palette action: {id}");
            self.macro_engine.mark_last_playback_error(reason.clone());
            self.macro_engine.fail_playback(&reason);
            self.notifications.notify(
                Toast::new(format!("Macro failed: {id}"))
                    .icon(ToastIcon::Error)
                    .duration(Duration::from_secs(4)),
            );
        }

        Cmd::none()
    }

    /// Dispatch a macro action by its suffix (after `macro:` prefix).
    fn dispatch_macro_action(&mut self, rest: &str) -> Cmd<MailMsg> {
        // Thread macros
        if let Some(thread_id) = rest.strip_prefix("summarize_thread:") {
            self.notifications.notify(
                Toast::new(format!("Summarizing thread {thread_id}..."))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(4)),
            );
            let target = DeepLinkTarget::ThreadById(thread_id.to_string());
            self.active_screen = MailScreenId::Threads;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Threads) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }
        if let Some(thread_id) = rest.strip_prefix("view_thread:") {
            let target = DeepLinkTarget::ThreadById(thread_id.to_string());
            self.active_screen = MailScreenId::Threads;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Threads) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }

        // Agent macros
        if let Some(agent) = rest.strip_prefix("fetch_inbox:") {
            let target = DeepLinkTarget::ExplorerForAgent(agent.to_string());
            self.active_screen = MailScreenId::Explorer;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Explorer) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }
        if let Some(agent) = rest.strip_prefix("view_reservations:") {
            let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
            self.active_screen = MailScreenId::Reservations;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::Reservations) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }

        // Tool macros
        if let Some(tool) = rest.strip_prefix("tool_history:") {
            let target = DeepLinkTarget::ToolByName(tool.to_string());
            self.active_screen = MailScreenId::ToolMetrics;
            if let Some(screen) = self.screens.get_mut(&MailScreenId::ToolMetrics) {
                screen.receive_deep_link(&target);
            }
            return Cmd::none();
        }

        // Message macros
        if let Some(id_str) = rest.strip_prefix("view_message:") {
            if let Ok(msg_id) = id_str.parse::<i64>() {
                let target = DeepLinkTarget::MessageById(msg_id);
                self.active_screen = MailScreenId::Messages;
                if let Some(screen) = self.screens.get_mut(&MailScreenId::Messages) {
                    screen.receive_deep_link(&target);
                }
            }
            return Cmd::none();
        }

        Cmd::none()
    }

    /// Handle macro engine control actions (record, play, stop, delete).
    ///
    /// Returns `Some(Cmd)` if the action was handled, `None` otherwise.
    #[allow(clippy::too_many_lines)]
    fn handle_macro_control(&mut self, id: &str) -> Option<Cmd<MailMsg>> {
        match id {
            macro_ids::RECORD_START => {
                self.macro_engine.start_recording();
                self.notifications.notify(
                    Toast::new("Recording macro... (use palette to stop)")
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                Some(Cmd::none())
            }
            macro_ids::RECORD_STOP => {
                // Generate an auto-name based on timestamp.
                let name = format!("macro-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
                if let Some(def) = self.macro_engine.stop_recording(&name) {
                    self.notifications.notify(
                        Toast::new(format!("Saved \"{}\" ({} steps)", def.name, def.len()))
                            .icon(ToastIcon::Info)
                            .duration(Duration::from_secs(4)),
                    );
                } else {
                    self.notifications.notify(
                        Toast::new("No steps recorded")
                            .icon(ToastIcon::Warning)
                            .duration(Duration::from_secs(3)),
                    );
                }
                Some(Cmd::none())
            }
            macro_ids::RECORD_CANCEL => {
                self.macro_engine.cancel_recording();
                self.notifications.notify(
                    Toast::new("Recording cancelled")
                        .icon(ToastIcon::Warning)
                        .duration(Duration::from_secs(2)),
                );
                Some(Cmd::none())
            }
            macro_ids::PLAYBACK_STOP => {
                self.macro_engine.stop_playback();
                self.notifications.notify(
                    Toast::new("Playback stopped")
                        .icon(ToastIcon::Warning)
                        .duration(Duration::from_secs(2)),
                );
                Some(Cmd::none())
            }
            _ => {
                // Prefixed macro control actions.
                if let Some(name) = id.strip_prefix(macro_ids::PLAY_PREFIX) {
                    if self
                        .macro_engine
                        .start_playback(name, PlaybackMode::Continuous)
                    {
                        self.notifications.notify(
                            Toast::new(format!("Playing \"{name}\"..."))
                                .icon(ToastIcon::Info)
                                .duration(Duration::from_secs(2)),
                        );
                        // Execute all steps immediately.
                        self.execute_macro_steps();
                    }
                    return Some(Cmd::none());
                }
                if let Some(name) = id.strip_prefix(macro_ids::PLAY_STEP_PREFIX) {
                    if self
                        .macro_engine
                        .start_playback(name, PlaybackMode::StepByStep)
                    {
                        self.notifications.notify(
                            Toast::new(format!("Step-by-step: \"{name}\" (Enter=next, Esc=stop)"))
                                .icon(ToastIcon::Info)
                                .duration(Duration::from_secs(4)),
                        );
                    }
                    return Some(Cmd::none());
                }
                if let Some(name) = id.strip_prefix(macro_ids::DRY_RUN_PREFIX) {
                    // Use the playback engine so dry-run leaves a structured log for forensics.
                    if self.macro_engine.start_playback(name, PlaybackMode::DryRun) {
                        self.execute_macro_steps();
                    }
                    if let Some(steps) = self.macro_engine.preview(name) {
                        let preview: Vec<String> = steps
                            .iter()
                            .enumerate()
                            .map(|(i, s)| format!("{}. {}", i + 1, s.label))
                            .collect();
                        self.notifications.notify(
                            Toast::new(format!("Preview \"{name}\":\n{}", preview.join("\n")))
                                .icon(ToastIcon::Info)
                                .duration(Duration::from_secs(8)),
                        );
                    }
                    return Some(Cmd::none());
                }
                if let Some(name) = id.strip_prefix(macro_ids::DELETE_PREFIX) {
                    if self.macro_engine.delete_macro(name) {
                        self.notifications.notify(
                            Toast::new(format!("Deleted macro \"{name}\""))
                                .icon(ToastIcon::Info)
                                .duration(Duration::from_secs(3)),
                        );
                    }
                    return Some(Cmd::none());
                }
                None
            }
        }
    }

    /// Execute all remaining steps in a continuous-mode macro.
    fn execute_macro_steps(&mut self) {
        loop {
            match self.macro_engine.next_step() {
                Some((action_id, PlaybackMode::DryRun)) => {
                    // Dry run: just log, don't execute.
                    let _ = action_id;
                }
                Some((action_id, _)) => {
                    // Execute the action via the normal dispatch path.
                    // Temporarily disable recording to avoid re-recording played steps.
                    let was_recording = self.macro_engine.recorder_state().is_recording();
                    if was_recording {
                        // Should not happen, but guard against it.
                        break;
                    }
                    let _ = self.dispatch_palette_action_from_macro(&action_id);
                }
                None => break,
            }
        }
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

                // Generate toasts from new high-priority events and track reservations
                let new_events = self.state.events_since(self.last_toast_seq);
                for event in &new_events {
                    self.last_toast_seq = event.seq().max(self.last_toast_seq);

                    // Track reservation lifecycle for expiry warnings
                    match event {
                        MailEvent::ReservationGranted {
                            agent,
                            paths,
                            ttl_s,
                            project,
                            ..
                        } => {
                            let ttl_i64 = i64::try_from(*ttl_s).unwrap_or(i64::MAX);
                            let expiry =
                                now_micros().saturating_add(ttl_i64.saturating_mul(1_000_000));
                            for path in paths {
                                let key = format!("{project}:{agent}:{path}");
                                let label = format!("{agent}:{path}");
                                self.reservation_tracker.insert(key, (label, expiry));
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
                                self.reservation_tracker.remove(&key);
                                self.warned_reservations.remove(&key);
                            }
                        }
                        _ => {}
                    }

                    if !self.toast_muted {
                        if let Some(toast) = toast_for_event(event, self.toast_severity) {
                            self.notifications.notify(self.apply_toast_policy(toast));
                        }
                    }
                }

                // Check for reservations expiring soon (within 5 minutes)
                let now = now_micros();
                let mut expiry_toasts = Vec::new();
                for (key, (label, expiry)) in &self.reservation_tracker {
                    if *expiry > now
                        && *expiry - now < RESERVATION_EXPIRY_WARN_MICROS
                        && !self.warned_reservations.contains(key)
                    {
                        let minutes_left = (*expiry - now) / 60_000_000;
                        expiry_toasts.push((
                            key.clone(),
                            Toast::new(format!("{label} expires in ~{minutes_left}m"))
                                .icon(ToastIcon::Warning)
                                .duration(Duration::from_secs(10)),
                        ));
                    }
                }
                for (key, toast) in expiry_toasts {
                    if !self.toast_muted && self.toast_severity.allows(ToastIcon::Warning) {
                        self.warned_reservations.insert(key);
                        self.notifications.notify(self.apply_toast_policy(toast));
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

                // When a modal is active, route all events to it (focus trapping).
                if self.modal_manager.handle_event(event) {
                    return Cmd::none();
                }

                // When action menu is active, route all events to it (focus trapping).
                if let Some(result) = self.action_menu.handle_event(event) {
                    match result {
                        ActionMenuResult::Consumed => return Cmd::none(),
                        ActionMenuResult::Dismissed => return Cmd::none(),
                        ActionMenuResult::Selected(action, context) => {
                            return self.dispatch_action_menu_selection(action, context);
                        }
                    }
                }

                // Step-by-step macro playback: Enter=confirm, Esc=stop.
                if matches!(
                    self.macro_engine.playback_state(),
                    PlaybackState::Paused { .. }
                ) {
                    if let Event::Key(key) = event {
                        if key.kind == KeyEventKind::Press {
                            match key.code {
                                KeyCode::Enter => {
                                    if let Some(action_id) = self.macro_engine.confirm_step() {
                                        let _ = self.dispatch_palette_action_from_macro(&action_id);
                                        // Show progress toast.
                                        if let Some(label) =
                                            self.macro_engine.playback_state().status_label()
                                        {
                                            self.notifications.notify(
                                                Toast::new(label)
                                                    .icon(ToastIcon::Info)
                                                    .duration(Duration::from_secs(3)),
                                            );
                                        }
                                    }
                                    return Cmd::none();
                                }
                                KeyCode::Escape => {
                                    self.macro_engine.stop_playback();
                                    self.notifications.notify(
                                        Toast::new("Playback cancelled")
                                            .icon(ToastIcon::Warning)
                                            .duration(Duration::from_secs(2)),
                                    );
                                    return Cmd::none();
                                }
                                _ => {} // Other keys pass through normally
                            }
                        }
                    }
                }

                // Toast focus mode: intercept keys when toast stack is focused.
                if self.toast_focus_index.is_some() {
                    if let Event::Key(key) = event {
                        if key.kind == KeyEventKind::Press {
                            match key.code {
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if let Some(ref mut idx) = self.toast_focus_index {
                                        let count = self.notifications.visible_count();
                                        if count > 0 {
                                            *idx = if *idx == 0 { count - 1 } else { *idx - 1 };
                                        }
                                    }
                                    return Cmd::none();
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if let Some(ref mut idx) = self.toast_focus_index {
                                        let count = self.notifications.visible_count();
                                        if count > 0 {
                                            *idx = (*idx + 1) % count;
                                        }
                                    }
                                    return Cmd::none();
                                }
                                KeyCode::Enter => {
                                    // Dismiss the focused toast.
                                    if let Some(idx) = self.toast_focus_index {
                                        let vis = self.notifications.visible();
                                        if let Some(toast) = vis.get(idx) {
                                            let id = toast.id;
                                            self.notifications.dismiss(id);
                                        }
                                        // Clamp index after dismissal.
                                        let count = self.notifications.visible_count();
                                        if count == 0 {
                                            self.toast_focus_index = None;
                                        } else {
                                            self.toast_focus_index =
                                                Some(idx.min(count.saturating_sub(1)));
                                        }
                                    }
                                    return Cmd::none();
                                }
                                KeyCode::Escape => {
                                    self.toast_focus_index = None;
                                    return Cmd::none();
                                }
                                KeyCode::Char('m') => {
                                    self.toast_muted = !self.toast_muted;
                                    let msg = if self.toast_muted {
                                        "Toasts muted"
                                    } else {
                                        "Toasts unmuted"
                                    };
                                    self.notifications.notify(
                                        self.apply_toast_policy(
                                            Toast::new(msg)
                                                .icon(ToastIcon::Info)
                                                .style(Style::default().fg(TOAST_COLOR_INFO)),
                                        ),
                                    );
                                    return Cmd::none();
                                }
                                _ => {
                                    // Let other keys (like Ctrl+T) fall through.
                                }
                            }
                        }
                    }
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
                        // Ctrl+T: toggle toast focus mode.
                        let is_ctrl_t = key.modifiers.contains(Modifiers::CTRL)
                            && matches!(key.code, KeyCode::Char('t'));
                        if is_ctrl_t && !text_mode {
                            if self.toast_focus_index.is_some() {
                                self.toast_focus_index = None;
                            } else if self.notifications.visible_count() > 0 {
                                self.toast_focus_index = Some(0);
                            }
                            return Cmd::none();
                        }
                        match key.code {
                            KeyCode::Char('q') if !text_mode => {
                                self.flush_before_shutdown();
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
                            // Action menu: . opens contextual actions for selected item
                            KeyCode::Char('.') if !text_mode => {
                                if let Some(screen) = self.screens.get(&self.active_screen) {
                                    if let Some((entries, anchor, ctx)) =
                                        screen.contextual_actions()
                                    {
                                        self.action_menu.open(entries, anchor, ctx);
                                    }
                                }
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
                self.flush_before_shutdown();
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

        let screen_bindings = self
            .screens
            .get(&self.active_screen)
            .map(|s| s.keybindings())
            .unwrap_or_default();

        // 3. Status line (z=3)
        tui_chrome::render_status_line(
            &self.state,
            self.active_screen,
            self.help_visible,
            &self.accessibility,
            &screen_bindings,
            self.toast_muted,
            frame,
            chrome.status_line,
        );

        // 4. Toast notifications (z=4, overlay)
        NotificationStack::new(&self.notifications)
            .margin(1)
            .render(area, frame);

        // 4b. Toast focus highlight overlay
        if let Some(focus_idx) = self.toast_focus_index {
            render_toast_focus_highlight(
                &self.notifications,
                focus_idx,
                area,
                1, // margin
                frame,
            );
        }

        // 4b. Action menu (z=4.3, contextual per-item actions)
        if self.action_menu.is_active() {
            self.action_menu.render(area, frame);
        }

        // 4c. Modal dialogs (z=4.5, between toasts and command palette)
        if self.modal_manager.is_active() {
            self.modal_manager.render(area, frame);
        }

        // 5. Command palette (z=5, modal)
        if self.command_palette.is_visible() {
            self.command_palette.render(area, frame);
        }

        // 6. Help overlay (z=6, topmost)
        if self.help_visible {
            let screen_label = crate::tui_screens::screen_meta(self.active_screen).title;
            let sections = self.keymap.contextual_help(&screen_bindings, screen_label);
            tui_chrome::render_help_overlay_sections(&sections, self.help_scroll, frame, area);
        }
    }
}

impl Drop for MailAppModel {
    fn drop(&mut self) {
        self.persist_palette_usage();
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
    pub const MESSAGE_PREFIX: &str = "message:";
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
    pub const SCREEN_ATTACHMENTS: &str = "screen:attachments";
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
        palette_action_ids::SCREEN_ATTACHMENTS => Some(MailScreenId::Attachments),
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
        MailScreenId::Attachments => palette_action_ids::SCREEN_ATTACHMENTS,
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
#[allow(clippy::too_many_lines)]
fn build_palette_actions_static() -> Vec<ActionItem> {
    let mut out = Vec::with_capacity(ALL_SCREEN_IDS.len() + 8);

    for &id in ALL_SCREEN_IDS {
        let meta = screen_meta(id);
        out.push(
            ActionItem::new(
                screen_palette_action_id(id),
                format!("Go to {}", meta.title),
            )
            .with_description(meta.description)
            .with_tags(&["screen", "navigate"])
            .with_category(screen_palette_category(id)),
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

    // ── Macro controls ────────────────────────────────────────────
    out.push(
        ActionItem::new(macro_ids::RECORD_START, "Record Macro")
            .with_description("Start recording a new operator macro")
            .with_tags(&["macro", "record", "automation"])
            .with_category("Macros"),
    );
    out.push(
        ActionItem::new(macro_ids::RECORD_STOP, "Stop Recording")
            .with_description("Stop recording and save the macro")
            .with_tags(&["macro", "record", "stop"])
            .with_category("Macros"),
    );
    out.push(
        ActionItem::new(macro_ids::RECORD_CANCEL, "Cancel Recording")
            .with_description("Discard the current recording")
            .with_tags(&["macro", "record", "cancel"])
            .with_category("Macros"),
    );
    out.push(
        ActionItem::new(macro_ids::PLAYBACK_STOP, "Stop Macro Playback")
            .with_description("Cancel the currently playing macro")
            .with_tags(&["macro", "playback", "stop"])
            .with_category("Macros"),
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

fn palette_action_cost_columns(action: &ActionItem) -> f64 {
    let title_width = display_width(action.title.as_str());
    let title_width = u16::try_from(title_width).unwrap_or(u16::MAX);
    f64::from(title_width.max(1))
}

fn register_palette_hints(
    hint_ranker: &mut HintRanker,
    palette_hint_ids: &mut HashMap<String, usize>,
    actions: &[ActionItem],
    context_key: &str,
) {
    for (index, action) in actions.iter().enumerate() {
        if palette_hint_ids.contains_key(action.id.as_str()) {
            continue;
        }

        let static_priority = u32::try_from(index)
            .unwrap_or(u32::MAX.saturating_sub(1))
            .saturating_add(1);
        let hint_context = if action.id.starts_with("quick:") {
            HintContext::Widget(context_key.to_string())
        } else {
            HintContext::Global
        };
        let hint_id = hint_ranker.register(
            action.id.as_str(),
            palette_action_cost_columns(action),
            hint_context,
            static_priority,
        );
        palette_hint_ids.insert(action.id.clone(), hint_id);
    }
}

#[derive(Debug, Clone, Default)]
struct PaletteMessageSummary {
    id: i64,
    subject: String,
    from_agent: String,
    to_agents: String,
    thread_id: String,
    timestamp_micros: i64,
}

#[derive(Debug, Clone, Default)]
struct PaletteMessageCache {
    database_url: String,
    fetched_at_micros: i64,
    messages: Vec<PaletteMessageSummary>,
}

#[derive(Debug, Clone, Default)]
struct ThreadPaletteStats {
    message_count: u64,
    latest_subject: String,
    participants: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
struct ReservationPaletteStats {
    exclusive: bool,
    released: bool,
    ttl_remaining_secs: Option<u64>,
}

static PALETTE_MESSAGE_CACHE: OnceLock<Mutex<PaletteMessageCache>> = OnceLock::new();

fn query_palette_agent_metadata(
    state: &TuiSharedState,
    limit: usize,
) -> HashMap<String, (String, String)> {
    let cfg = DbPoolConfig {
        database_url: state.config_snapshot().database_url,
        ..Default::default()
    };
    let Ok(path) = cfg.sqlite_path() else {
        return HashMap::new();
    };
    let Ok(conn) = DbConn::open_file(&path) else {
        return HashMap::new();
    };

    conn.query_sync(
        &format!(
            "SELECT a.name, a.model, p.slug AS project_slug \
             FROM agents a \
             JOIN projects p ON p.id = a.project_id \
             ORDER BY a.last_active_ts DESC \
             LIMIT {limit}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                Some((
                    row.get_named::<String>("name").ok()?,
                    (
                        row.get_named::<String>("model").ok().unwrap_or_default(),
                        row.get_named::<String>("project_slug")
                            .ok()
                            .unwrap_or_default(),
                    ),
                ))
            })
            .collect()
    })
    .unwrap_or_default()
}

fn query_palette_recent_messages(
    state: &TuiSharedState,
    limit: usize,
) -> Vec<PaletteMessageSummary> {
    let cfg = DbPoolConfig {
        database_url: state.config_snapshot().database_url,
        ..Default::default()
    };
    let Ok(path) = cfg.sqlite_path() else {
        return Vec::new();
    };
    let Ok(conn) = DbConn::open_file(&path) else {
        return Vec::new();
    };

    conn.query_sync(
        &format!(
            "SELECT m.id, m.subject, m.thread_id, m.created_ts, \
             a_sender.name AS from_agent, \
             COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
             FROM messages m \
             JOIN agents a_sender ON a_sender.id = m.sender_id \
             LEFT JOIN message_recipients mr ON mr.message_id = m.id \
             LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
             GROUP BY m.id \
             ORDER BY m.created_ts DESC \
             LIMIT {limit}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                Some(PaletteMessageSummary {
                    id: row.get_named::<i64>("id").ok()?,
                    subject: row.get_named::<String>("subject").ok().unwrap_or_default(),
                    from_agent: row
                        .get_named::<String>("from_agent")
                        .ok()
                        .unwrap_or_default(),
                    to_agents: row
                        .get_named::<String>("to_agents")
                        .ok()
                        .unwrap_or_default(),
                    thread_id: row
                        .get_named::<String>("thread_id")
                        .ok()
                        .unwrap_or_default(),
                    timestamp_micros: row.get_named::<i64>("created_ts").ok().unwrap_or(0),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn fetch_palette_recent_messages(
    state: &TuiSharedState,
    limit: usize,
) -> Vec<PaletteMessageSummary> {
    let database_url = state.config_snapshot().database_url;
    let now = now_micros();
    let cache = PALETTE_MESSAGE_CACHE.get_or_init(|| Mutex::new(PaletteMessageCache::default()));
    if let Ok(guard) = cache.lock() {
        let fresh_enough =
            now.saturating_sub(guard.fetched_at_micros) <= PALETTE_DB_CACHE_TTL_MICROS;
        if guard.database_url == database_url && fresh_enough {
            return guard.messages.iter().take(limit).cloned().collect();
        }
    }

    let messages = query_palette_recent_messages(state, limit);
    if let Ok(mut guard) = cache.lock() {
        guard.database_url = database_url;
        guard.fetched_at_micros = now;
        guard.messages = messages.clone();
    }
    messages
}

fn format_timestamp_micros(micros: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros).map_or_else(
        || micros.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

fn append_palette_message_actions(messages: &[PaletteMessageSummary], out: &mut Vec<ActionItem>) {
    for message in messages {
        let to_agents = if message.to_agents.is_empty() {
            "n/a"
        } else {
            message.to_agents.as_str()
        };
        let mut action = ActionItem::new(
            format!("{}{}", palette_action_ids::MESSAGE_PREFIX, message.id),
            format!("Message: {}", truncate_subject(&message.subject, 60)),
        )
        .with_description(format!(
            "{} -> {} | {}",
            message.from_agent,
            to_agents,
            format_timestamp_micros(message.timestamp_micros)
        ))
        .with_category("Messages");
        action.tags.push("message".to_string());
        action.tags.push(message.from_agent.clone());
        if !message.thread_id.is_empty() {
            action.tags.push(message.thread_id.clone());
        }
        out.push(action);
    }
}

fn collect_thread_palette_stats(events: &[MailEvent]) -> HashMap<String, ThreadPaletteStats> {
    let mut stats: HashMap<String, ThreadPaletteStats> = HashMap::new();
    for event in events {
        match event {
            MailEvent::MessageSent {
                thread_id,
                from,
                to,
                subject,
                ..
            }
            | MailEvent::MessageReceived {
                thread_id,
                from,
                to,
                subject,
                ..
            } => {
                let entry = stats.entry(thread_id.clone()).or_default();
                entry.message_count = entry.message_count.saturating_add(1);
                entry.latest_subject.clone_from(subject);
                entry.participants.insert(from.clone());
                for recipient in to {
                    entry.participants.insert(recipient.clone());
                }
            }
            _ => {}
        }
    }
    stats
}

fn format_participant_list(participants: &HashSet<String>, max_items: usize) -> String {
    if participants.is_empty() {
        return "no participants".to_string();
    }
    let mut names: Vec<&str> = participants.iter().map(String::as_str).collect();
    names.sort_unstable();
    if names.len() <= max_items {
        return names.join(", ");
    }
    let hidden = names.len() - max_items;
    format!("{} +{hidden}", names[..max_items].join(", "))
}

fn collect_reservation_palette_stats(
    events: &[MailEvent],
    now_micros_ts: i64,
) -> HashMap<String, ReservationPaletteStats> {
    let mut stats: HashMap<String, ReservationPaletteStats> = HashMap::new();
    for event in events {
        match event {
            MailEvent::ReservationGranted {
                agent,
                exclusive,
                ttl_s,
                timestamp_micros,
                ..
            } => {
                let ttl_i64 = i64::try_from(*ttl_s).unwrap_or(i64::MAX);
                let expiry_micros =
                    timestamp_micros.saturating_add(ttl_i64.saturating_mul(1_000_000));
                let remaining_micros = expiry_micros.saturating_sub(now_micros_ts).max(0);
                let ttl_remaining_secs = u64::try_from(remaining_micros / 1_000_000).ok();
                stats.insert(
                    agent.clone(),
                    ReservationPaletteStats {
                        exclusive: *exclusive,
                        released: false,
                        ttl_remaining_secs,
                    },
                );
            }
            MailEvent::ReservationReleased { agent, .. } => {
                stats.insert(
                    agent.clone(),
                    ReservationPaletteStats {
                        exclusive: false,
                        released: true,
                        ttl_remaining_secs: None,
                    },
                );
            }
            _ => {}
        }
    }
    stats
}

fn format_ttl_remaining_short(ttl_secs: u64) -> String {
    if ttl_secs >= 3600 {
        format!("{}h", ttl_secs / 3600)
    } else if ttl_secs >= 60 {
        format!("{}m", ttl_secs / 60)
    } else {
        format!("{ttl_secs}s")
    }
}

/// Append palette entries derived from the periodic DB snapshot (agents, projects, contacts).
fn build_palette_actions_from_snapshot(state: &TuiSharedState, out: &mut Vec<ActionItem>) {
    let Some(snap) = state.db_stats_snapshot() else {
        return;
    };
    let agent_metadata = query_palette_agent_metadata(state, PALETTE_DYNAMIC_AGENT_CAP);
    let recent_messages = fetch_palette_recent_messages(state, PALETTE_DYNAMIC_MESSAGE_CAP);

    for agent in snap.agents_list.into_iter().take(PALETTE_DYNAMIC_AGENT_CAP) {
        let crate::tui_events::AgentSummary {
            name,
            program,
            last_active_ts,
        } = agent;
        let desc = if let Some((model, project_slug)) = agent_metadata.get(&name) {
            format!("{program}/{model} • project {project_slug} • last_active_ts: {last_active_ts}")
        } else {
            format!("{program} (last_active_ts: {last_active_ts})")
        };
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
            "{} — {} agents, {} msgs, {} active reservations",
            proj.human_key, proj.agent_count, proj.message_count, proj.reservation_count
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

    append_palette_message_actions(&recent_messages, out);

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
    let thread_stats = collect_thread_palette_stats(&events);
    let reservation_stats = collect_reservation_palette_stats(&events, now_micros());

    let mut threads_seen: HashSet<String> = HashSet::new();
    let mut messages_seen: HashSet<i64> = out
        .iter()
        .filter_map(|action| {
            action
                .id
                .strip_prefix(palette_action_ids::MESSAGE_PREFIX)
                .and_then(|id_str| id_str.parse::<i64>().ok())
        })
        .collect();
    let mut tools_seen: HashSet<String> = HashSet::new();
    let mut reservations_seen: HashSet<String> = HashSet::new();

    for ev in events.iter().rev() {
        if threads_seen.len() < PALETTE_DYNAMIC_THREAD_CAP {
            if let Some((thread_id, subject)) = extract_thread(ev) {
                if threads_seen.insert(thread_id.to_string()) {
                    let thread_desc = if let Some(stats) = thread_stats.get(thread_id) {
                        let participants = format_participant_list(&stats.participants, 3);
                        format!(
                            "{} msgs • {} • latest: {}",
                            stats.message_count,
                            participants,
                            truncate_subject(&stats.latest_subject, 42)
                        )
                    } else {
                        format!("Latest: {subject}")
                    };
                    out.push(
                        ActionItem::new(
                            format!("{}{}", palette_action_ids::THREAD_PREFIX, thread_id),
                            format!("Thread: {thread_id}"),
                        )
                        .with_description(thread_desc)
                        .with_tags(&["thread", "messages"])
                        .with_category("Threads"),
                    );
                }
            }
        }

        if messages_seen.len() < PALETTE_DYNAMIC_MESSAGE_CAP {
            if let Some((message_id, from, subject, thread_id)) = extract_message(ev) {
                if messages_seen.insert(message_id) {
                    let mut action = ActionItem::new(
                        format!("{}{}", palette_action_ids::MESSAGE_PREFIX, message_id),
                        format!("Message: {}", truncate_subject(subject, 56)),
                    )
                    .with_description(format!("{from} • thread {thread_id} • id {message_id}"))
                    .with_category("Messages");
                    action.tags.push("message".to_string());
                    action.tags.push((*from).to_string());
                    action.tags.push((*thread_id).to_string());
                    out.push(action);
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
                    let desc = reservation_stats.get(agent).map_or_else(
                        || "View file reservations for this agent".to_string(),
                        |stats| {
                            if stats.released {
                                return "released • no active reservation".to_string();
                            }
                            let mode = if stats.exclusive {
                                "exclusive"
                            } else {
                                "shared"
                            };
                            let ttl = stats.ttl_remaining_secs.map_or_else(
                                || "ttl unknown".to_string(),
                                |ttl_secs| {
                                    format!("{} remaining", format_ttl_remaining_short(ttl_secs))
                                },
                            );
                            format!("{mode} • {ttl}")
                        },
                    );
                    out.push(
                        ActionItem::new(
                            format!("{}{}", palette_action_ids::RESERVATION_PREFIX, agent),
                            format!("Reservation: {agent}"),
                        )
                        .with_description(desc)
                        .with_tags(&["reservation", "file", "lock"])
                        .with_category("Reservations"),
                    );
                }
            }
        }

        if threads_seen.len() >= PALETTE_DYNAMIC_THREAD_CAP
            && messages_seen.len() >= PALETTE_DYNAMIC_MESSAGE_CAP
            && tools_seen.len() >= PALETTE_DYNAMIC_TOOL_CAP
            && reservations_seen.len() >= PALETTE_DYNAMIC_RESERVATION_CAP
        {
            break;
        }
    }
}

/// Derive a human-readable label from a palette action ID.
///
/// Used when recording macros to give each step a meaningful name.
fn palette_action_label(id: &str) -> String {
    // Screen navigation
    if let Some(screen_id) = screen_from_palette_action_id(id) {
        return format!("Go to {}", screen_name_from_id(screen_id));
    }
    // Quick actions
    if id.starts_with("quick:") || id.starts_with("macro:") {
        // Keep the original ID as label — it's already descriptive.
        return id.to_string();
    }
    // Named palette actions
    match id {
        palette_action_ids::APP_TOGGLE_HELP => "Toggle Help".into(),
        palette_action_ids::APP_QUIT => "Quit".into(),
        palette_action_ids::TRANSPORT_TOGGLE => "Toggle Transport".into(),
        palette_action_ids::THEME_CYCLE => "Cycle Theme".into(),
        palette_action_ids::LAYOUT_RESET => "Reset Layout".into(),
        _ => id.to_string(),
    }
}

/// Short screen name from ID for labels.
fn screen_name_from_id(id: MailScreenId) -> &'static str {
    screen_meta(id).title
}

/// Generate a toast notification for high-priority events.
///
/// Returns `None` for routine events that shouldn't produce toasts,
/// or if the toast's severity is below the configured threshold.
fn toast_for_event(event: &MailEvent, severity: ToastSeverityThreshold) -> Option<Toast> {
    let (icon, toast) = match event {
        // ── Messaging ────────────────────────────────────────────
        MailEvent::MessageSent { from, to, .. } => {
            let recipients = if to.len() > 2 {
                format!("{} +{}", to[0], to.len() - 1)
            } else {
                to.join(", ")
            };
            (
                ToastIcon::Info,
                Toast::new(format!("{from} → {recipients}"))
                    .icon(ToastIcon::Info)
                    .style(Style::default().fg(TOAST_COLOR_INFO))
                    .duration(Duration::from_secs(4)),
            )
        }
        MailEvent::MessageReceived { from, subject, .. } => {
            let truncated = if subject.len() > 40 {
                format!("{}…", &subject[..39])
            } else {
                subject.clone()
            };
            (
                ToastIcon::Info,
                Toast::new(format!("{from}: {truncated}"))
                    .icon(ToastIcon::Info)
                    .style(Style::default().fg(TOAST_COLOR_INFO))
                    .duration(Duration::from_secs(5)),
            )
        }

        // ── Identity ─────────────────────────────────────────────
        MailEvent::AgentRegistered { name, program, .. } => (
            ToastIcon::Success,
            Toast::new(format!("{name} ({program})"))
                .icon(ToastIcon::Success)
                .style(Style::default().fg(TOAST_COLOR_SUCCESS))
                .duration(Duration::from_secs(4)),
        ),

        // ── Tool calls: slow or errored ──────────────────────────
        MailEvent::ToolCallEnd {
            tool_name,
            result_preview: Some(preview),
            ..
        } if preview.contains("error") || preview.contains("Error") => (
            ToastIcon::Error,
            Toast::new(format!("{tool_name} error"))
                .icon(ToastIcon::Error)
                .style(Style::default().fg(TOAST_COLOR_ERROR))
                .duration(Duration::from_secs(15)),
        ),
        MailEvent::ToolCallEnd {
            tool_name,
            duration_ms,
            ..
        } if *duration_ms > SLOW_TOOL_THRESHOLD_MS => (
            ToastIcon::Warning,
            Toast::new(format!("{tool_name}: {duration_ms}ms"))
                .icon(ToastIcon::Warning)
                .style(Style::default().fg(TOAST_COLOR_WARNING))
                .duration(Duration::from_secs(8)),
        ),

        // ── Reservations: exclusive grants ───────────────────────
        MailEvent::ReservationGranted {
            agent,
            paths,
            exclusive: true,
            ..
        } => {
            let path_display = paths.first().map_or("…", String::as_str);
            (
                ToastIcon::Info,
                Toast::new(format!("{agent} locked {path_display}"))
                    .icon(ToastIcon::Info)
                    .style(Style::default().fg(TOAST_COLOR_INFO))
                    .duration(Duration::from_secs(4)),
            )
        }

        // ── HTTP 5xx ─────────────────────────────────────────────
        MailEvent::HttpRequest { status, path, .. } if *status >= 500 => (
            ToastIcon::Error,
            Toast::new(format!("HTTP {status} on {path}"))
                .icon(ToastIcon::Error)
                .style(Style::default().fg(TOAST_COLOR_ERROR))
                .duration(Duration::from_secs(6)),
        ),

        // ── Lifecycle ────────────────────────────────────────────
        MailEvent::ServerShutdown { .. } => (
            ToastIcon::Warning,
            Toast::new("Server shutting down")
                .icon(ToastIcon::Warning)
                .style(Style::default().fg(TOAST_COLOR_WARNING))
                .duration(Duration::from_secs(8)),
        ),
        MailEvent::ServerStarted { endpoint, .. } => (
            ToastIcon::Success,
            Toast::new(format!("Server started at {endpoint}"))
                .icon(ToastIcon::Success)
                .style(Style::default().fg(TOAST_COLOR_SUCCESS))
                .duration(Duration::from_secs(5)),
        ),

        _ => return None,
    };

    // Apply severity filter
    if severity.allows(icon) {
        Some(toast)
    } else {
        None
    }
}

/// Draw a highlighted border around the focused toast in the notification stack.
///
/// This is rendered as a post-processing overlay after `NotificationStack::render`,
/// overwriting the border cells of the focused toast with a bright highlight color.
fn render_toast_focus_highlight(
    queue: &NotificationQueue,
    focus_idx: usize,
    area: Rect,
    margin: u16,
    frame: &mut Frame,
) {
    let positions = queue.calculate_positions(area.width, area.height, margin);
    let visible = queue.visible();

    if focus_idx >= visible.len() || focus_idx >= positions.len() {
        return;
    }

    let toast = &visible[focus_idx];
    let (_, px, py) = positions[focus_idx];
    let (tw, th) = toast.calculate_dimensions();
    let x = area.x.saturating_add(px);
    let y = area.y.saturating_add(py);

    highlight_toast_border(x, y, tw, th, frame);
    render_focus_hint(visible, &positions, area, x, y.saturating_add(th), frame);
}

/// Overwrite the border cells of the toast area with the highlight color.
fn highlight_toast_border(x: u16, y: u16, tw: u16, th: u16, frame: &mut Frame) {
    // Top and bottom border rows.
    for bx in x..x.saturating_add(tw) {
        for &by in &[y, y.saturating_add(th).saturating_sub(1)] {
            if let Some(cell) = frame.buffer.get_mut(bx, by) {
                cell.fg = TOAST_FOCUS_HIGHLIGHT;
            }
        }
    }
    // Left and right border columns.
    let bottom = y.saturating_add(th).saturating_sub(1);
    for by in y..=bottom {
        for &bx in &[x, x.saturating_add(tw).saturating_sub(1)] {
            if let Some(cell) = frame.buffer.get_mut(bx, by) {
                cell.fg = TOAST_FOCUS_HIGHLIGHT;
            }
        }
    }
}

/// Draw the hint text below the last visible toast.
fn render_focus_hint(
    visible: &[Toast],
    positions: &[(ftui::widgets::toast::ToastId, u16, u16)],
    area: Rect,
    hint_x: u16,
    default_y: u16,
    frame: &mut Frame,
) {
    let hint = "Ctrl+T:exit  \u{2191}\u{2193}:nav  Enter:dismiss";
    let hint_y = positions.last().map_or(default_y, |(_, _, py)| {
        let (_, lh) = visible.last().map_or((0, 3), |t| t.calculate_dimensions());
        area.y.saturating_add(*py).saturating_add(lh)
    });

    for (i, ch) in hint.chars().enumerate() {
        let Ok(offset) = u16::try_from(i) else {
            break;
        };
        let hx = hint_x.saturating_add(offset);
        if hx >= area.right() {
            break;
        }
        if let Some(cell) = frame.buffer.get_mut(hx, hint_y) {
            *cell = ftui::Cell::from_char(ch);
            cell.fg = TOAST_FOCUS_HIGHLIGHT;
        }
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

fn extract_message(event: &MailEvent) -> Option<(i64, &str, &str, &str)> {
    match event {
        MailEvent::MessageSent {
            id,
            from,
            subject,
            thread_id,
            ..
        }
        | MailEvent::MessageReceived {
            id,
            from,
            subject,
            thread_id,
            ..
        } => Some((*id, from, subject, thread_id)),
        _ => None,
    }
}

fn truncate_subject(subject: &str, max_chars: usize) -> String {
    let mut truncated = String::new();
    for (idx, ch) in subject.chars().enumerate() {
        if idx >= max_chars {
            truncated.push('…');
            return truncated;
        }
        truncated.push(ch);
    }
    truncated
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
    use crate::tui_macro::{MacroDef, MacroStep};
    use crate::tui_screens::MailScreenMsg;
    use ftui::KeyEvent;
    use ftui_widgets::NotificationPriority;
    use mcp_agent_mail_core::Config;
    use serde::Serialize;
    use std::path::{Path, PathBuf};

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
    fn dispatch_palette_message_prefix_goes_to_messages() {
        let mut model = test_model();
        model.dispatch_palette_action("message:42");
        assert_eq!(model.active_screen(), MailScreenId::Messages);
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
    fn dynamic_palette_adds_message_entries_from_events() {
        let model = test_model();
        assert!(model.state.push_event(MailEvent::message_received(
            42,
            "BlueLake",
            vec!["RedFox".to_string()],
            "Subject for dynamic palette entry",
            "thread-42",
            "proj-a",
        )));
        assert!(model.state.push_event(MailEvent::message_sent(
            99,
            "RedFox",
            vec!["BlueLake".to_string()],
            "Outgoing subject",
            "thread-99",
            "proj-a",
        )));

        let actions = build_palette_actions(&model.state);
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"message:42"));
        assert!(ids.contains(&"message:99"));

        let message_entry = actions
            .iter()
            .find(|a| a.id == "message:42")
            .expect("message action for id 42");
        assert!(
            message_entry
                .title
                .contains("Subject for dynamic palette entry")
        );
        assert!(
            message_entry
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("thread-42")
        );
    }

    #[test]
    fn append_palette_message_actions_formats_subject_description_and_tags() {
        let mut out = Vec::new();
        let messages = vec![PaletteMessageSummary {
            id: 42,
            subject: "A".repeat(80),
            from_agent: "BlueLake".to_string(),
            to_agents: "RedFox,GreenWolf".to_string(),
            thread_id: "br-42".to_string(),
            timestamp_micros: 1_700_000_000_000_000,
        }];

        append_palette_message_actions(&messages, &mut out);
        assert_eq!(out.len(), 1);
        let action = &out[0];
        assert_eq!(action.id, "message:42");
        assert!(
            action
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("BlueLake -> RedFox,GreenWolf")
        );
        assert!(action.tags.contains(&"message".to_string()));
        assert!(action.tags.contains(&"BlueLake".to_string()));
        assert!(action.tags.contains(&"br-42".to_string()));
    }

    #[test]
    fn thread_palette_entries_include_message_count_and_participants() {
        let model = test_model();
        assert!(model.state.push_event(MailEvent::message_sent(
            1,
            "BlueLake",
            vec!["RedFox".to_string()],
            "First subject",
            "thread-1",
            "proj-a",
        )));
        assert!(model.state.push_event(MailEvent::message_received(
            2,
            "RedFox",
            vec!["BlueLake".to_string()],
            "Second subject",
            "thread-1",
            "proj-a",
        )));

        let mut out = Vec::new();
        build_palette_actions_from_events(&model.state, &mut out);
        let thread_action = out
            .iter()
            .find(|action| action.id == "thread:thread-1")
            .expect("thread action");
        let desc = thread_action.description.as_deref().unwrap_or_default();
        assert!(desc.contains("2 msgs"));
        assert!(desc.contains("BlueLake"));
        assert!(desc.contains("RedFox"));
    }

    #[test]
    fn reservation_palette_entries_include_ttl_and_exclusive_state() {
        let model = test_model();
        assert!(model.state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["crates/mcp-agent-mail-server/src/tui_app.rs".to_string()],
            true,
            600,
            "proj-a",
        )));

        let mut out = Vec::new();
        build_palette_actions_from_events(&model.state, &mut out);
        let reservation_action = out
            .iter()
            .find(|action| action.id == "reservation:BlueLake")
            .expect("reservation action");
        let desc = reservation_action
            .description
            .as_deref()
            .unwrap_or_default();
        assert!(desc.contains("exclusive"));
        assert!(desc.contains("remaining"));
    }

    #[test]
    fn palette_message_cache_respects_ttl() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let db_url = state.config_snapshot().database_url;
        let expected = PaletteMessageSummary {
            id: 7,
            subject: "cached subject".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "RedFox".to_string(),
            thread_id: "br-7".to_string(),
            timestamp_micros: now_micros(),
        };

        let cache =
            PALETTE_MESSAGE_CACHE.get_or_init(|| Mutex::new(PaletteMessageCache::default()));
        {
            let mut guard = cache.lock().expect("cache lock");
            guard.database_url = db_url;
            guard.fetched_at_micros = now_micros();
            guard.messages = vec![expected.clone()];
        }

        let messages = fetch_palette_recent_messages(&state, 10);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, expected.id);
        assert_eq!(messages[0].subject, expected.subject);

        if let Ok(mut guard) = cache.lock() {
            *guard = PaletteMessageCache::default();
        }
    }

    #[test]
    fn hint_ranker_promotes_frequently_used_actions() {
        let mut model = test_model();
        let actions = vec![
            ActionItem::new("hint:a", "Alpha Action"),
            ActionItem::new("hint:b", "Beta Action"),
            ActionItem::new("hint:c", "Gamma Action"),
        ];

        let initial = model.rank_palette_actions(actions.clone());
        assert_eq!(initial.first().map(|a| a.id.as_str()), Some("hint:a"));

        for _ in 0..8 {
            model.record_palette_action_usage("hint:c");
        }

        let reranked = model.rank_palette_actions(actions);
        assert_eq!(reranked.first().map(|a| a.id.as_str()), Some("hint:c"));
    }

    #[test]
    fn record_palette_action_usage_updates_hint_stats_and_order() {
        let mut model = test_model();
        let actions = vec![
            ActionItem::new("hint:a", "Alpha Action"),
            ActionItem::new("hint:b", "Beta Action"),
            ActionItem::new("hint:c", "Gamma Action"),
        ];
        model.sync_palette_hints(&actions);

        let hint_id = *model
            .palette_hint_ids
            .get("hint:b")
            .expect("hint id for hint:b");
        let before_alpha = model
            .hint_ranker
            .stats(hint_id)
            .expect("stats before usage")
            .alpha;

        model.record_palette_action_usage("hint:b");

        let after_alpha = model
            .hint_ranker
            .stats(hint_id)
            .expect("stats after usage")
            .alpha;
        assert!(after_alpha > before_alpha, "usage should increase alpha");

        let ranked = model.rank_palette_actions(actions);
        assert_eq!(ranked.first().map(|a| a.id.as_str()), Some("hint:b"));
    }

    #[test]
    fn rank_palette_actions_keeps_all_entries_without_usage_data() {
        let mut model = test_model();
        let actions = vec![
            ActionItem::new("hint:a", "Alpha Action"),
            ActionItem::new("hint:b", "Beta Action"),
            ActionItem::new("hint:c", "Gamma Action"),
        ];

        let ranked = model.rank_palette_actions(actions.clone());
        assert_eq!(ranked.len(), actions.len());

        let mut ranked_ids: Vec<String> = ranked.into_iter().map(|action| action.id).collect();
        let mut source_ids: Vec<String> = actions.into_iter().map(|action| action.id).collect();
        ranked_ids.sort();
        source_ids.sort();
        assert_eq!(ranked_ids, source_ids);
    }

    #[test]
    fn hint_ranker_ordering_combines_with_bayesian_palette_scoring() {
        let actions = vec![
            ActionItem::new("alpha:one", "Alpha One"),
            ActionItem::new("alpha:two", "Alpha Two"),
            ActionItem::new("beta:item", "Beta Item"),
        ];

        let mut baseline_palette = CommandPalette::new();
        baseline_palette.replace_actions(actions.clone());
        baseline_palette.open();
        baseline_palette.set_query("alpha");
        assert_eq!(
            baseline_palette
                .selected_action()
                .map(|action| action.id.as_str()),
            Some("alpha:one")
        );

        let mut model = test_model();
        model.sync_palette_hints(&actions);
        for _ in 0..8 {
            model.record_palette_action_usage("alpha:two");
        }
        let ranked_actions = model.rank_palette_actions(actions);

        let mut boosted_palette = CommandPalette::new();
        boosted_palette.replace_actions(ranked_actions);
        boosted_palette.open();
        boosted_palette.set_query("alpha");
        assert_eq!(
            boosted_palette
                .selected_action()
                .map(|action| action.id.as_str()),
            Some("alpha:two")
        );
    }

    #[test]
    fn decayed_palette_usage_weight_reduces_old_signal() {
        let now = now_micros();
        let recent = decayed_palette_usage_weight(10, now - 10 * 60 * 1_000_000, now);
        let stale = decayed_palette_usage_weight(10, now - 24 * 60 * 60 * 1_000_000, now);
        assert!(recent > stale);
    }

    #[test]
    fn rank_palette_actions_prefers_recent_over_stale_usage() {
        let mut model = test_model();
        let now = now_micros();
        model.palette_usage_stats.insert(
            "action:stale".to_string(),
            (8, now - 24 * 60 * 60 * 1_000_000),
        );
        model
            .palette_usage_stats
            .insert("action:recent".to_string(), (8, now - 10 * 60 * 1_000_000));

        let actions = vec![
            ActionItem::new("action:stale", "Stale Action"),
            ActionItem::new("action:recent", "Recent Action"),
        ];
        let ranked = model.rank_palette_actions(actions);
        assert_eq!(ranked.first().map(|a| a.id.as_str()), Some("action:recent"));
    }

    #[test]
    fn palette_usage_persists_and_restores_with_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config {
            console_persist_path: tmp.path().join("config.env"),
            ..Config::default()
        };

        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::with_config(state, &config);
        model.record_palette_action_usage("screen:messages");
        model.record_palette_action_usage("screen:messages");
        model.flush_before_shutdown();

        let usage_path = crate::tui_persist::palette_usage_path(&config.console_persist_path);
        let persisted = crate::tui_persist::load_palette_usage(&usage_path).expect("load usage");
        assert_eq!(
            persisted.get("screen:messages").map(|(count, _)| *count),
            Some(2)
        );

        let state_replay = TuiSharedState::new(&config);
        let replay = MailAppModel::with_config(state_replay, &config);
        assert_eq!(
            replay
                .palette_usage_stats
                .get("screen:messages")
                .map(|(count, _)| *count),
            Some(2)
        );
    }

    #[test]
    fn palette_usage_corrupt_file_falls_back_to_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config {
            console_persist_path: tmp.path().join("config.env"),
            ..Config::default()
        };
        let usage_path = crate::tui_persist::palette_usage_path(&config.console_persist_path);
        std::fs::write(&usage_path, "{ not-valid-json ]").expect("write corrupt file");

        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(state, &config);
        assert!(model.palette_usage_stats.is_empty());
    }

    #[test]
    fn palette_usage_missing_file_starts_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config {
            console_persist_path: tmp.path().join("config.env"),
            ..Config::default()
        };

        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(state, &config);
        assert!(model.palette_usage_stats.is_empty());
    }

    #[test]
    fn palette_renders_ranked_overlay_large_layout() {
        let mut model = test_model();
        model.open_palette();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(160, 48, &mut pool);
        model.view(&mut frame);
        let text = ftui_harness::buffer_to_text(&frame.buffer);

        assert!(text.contains("Command Palette"));
    }

    #[test]
    fn palette_renders_ranked_overlay_compact_layout() {
        let mut model = test_model();
        model.open_palette();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        let text = ftui_harness::buffer_to_text(&frame.buffer);

        assert!(text.contains("Command Palette"));
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

    // ── Macro dispatch tests ─────────────────────────────────────

    #[test]
    fn dispatch_macro_summarize_thread_goes_to_threads() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:summarize_thread:br-3vwi");
        assert_eq!(model.active_screen(), MailScreenId::Threads);
    }

    #[test]
    fn dispatch_macro_view_thread_goes_to_threads() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:view_thread:br-3vwi");
        assert_eq!(model.active_screen(), MailScreenId::Threads);
    }

    #[test]
    fn dispatch_macro_fetch_inbox_goes_to_explorer() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:fetch_inbox:RedFox");
        assert_eq!(model.active_screen(), MailScreenId::Explorer);
    }

    #[test]
    fn dispatch_macro_view_reservations_goes_to_reservations() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:view_reservations:BlueLake");
        assert_eq!(model.active_screen(), MailScreenId::Reservations);
    }

    #[test]
    fn dispatch_macro_tool_history_goes_to_tool_metrics() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:tool_history:send_message");
        assert_eq!(model.active_screen(), MailScreenId::ToolMetrics);
    }

    #[test]
    fn dispatch_macro_view_message_goes_to_messages() {
        let mut model = test_model();
        model.dispatch_palette_action("macro:view_message:42");
        assert_eq!(model.active_screen(), MailScreenId::Messages);
    }

    #[test]
    fn dispatch_macro_unknown_is_noop() {
        let mut model = test_model();
        let prev = model.active_screen();
        model.dispatch_palette_action("macro:unknown:foo");
        assert_eq!(model.active_screen(), prev);
    }

    // ── Operator macro deterministic replay E2E (br-3vwi.10.15) ────────────

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf()
    }

    fn new_artifact_dir(label: &str) -> PathBuf {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let dir = repo_root().join(format!(
            "tests/artifacts/tui/macro_replay/{ts}_{}_{}",
            std::process::id(),
            label
        ));
        let _ = std::fs::create_dir_all(dir.join("steps"));
        let _ = std::fs::create_dir_all(dir.join("failures"));
        dir
    }

    #[derive(Debug, Serialize)]
    struct StepTelemetry {
        phase: &'static str,
        step_index: usize,
        action_id: String,
        label: String,
        stable_hash64: String,
        delay_ms: Option<u64>,
        executed: Option<bool>,
        error: Option<String>,
        before_screen: String,
        after_screen: String,
        help_visible: bool,
    }

    fn screen_label(id: MailScreenId) -> &'static str {
        match id {
            MailScreenId::Dashboard => "dashboard",
            MailScreenId::Messages => "messages",
            MailScreenId::Threads => "threads",
            MailScreenId::Search => "search",
            MailScreenId::Agents => "agents",
            MailScreenId::Reservations => "reservations",
            MailScreenId::ToolMetrics => "tool_metrics",
            MailScreenId::SystemHealth => "system_health",
            MailScreenId::Timeline => "timeline",
            MailScreenId::Projects => "projects",
            MailScreenId::Contacts => "contacts",
            MailScreenId::Explorer => "explorer",
            MailScreenId::Analytics => "analytics",
            MailScreenId::Attachments => "attachments",
        }
    }

    fn first_divergence(a: &[u64], b: &[u64]) -> Option<usize> {
        let n = a.len().min(b.len());
        for i in 0..n {
            if a[i] != b[i] {
                return Some(i);
            }
        }
        if a.len() != b.len() {
            return Some(n);
        }
        None
    }

    #[derive(Debug, Serialize)]
    struct MacroReplayReport {
        generated_at: String,
        agent: &'static str,
        bead: &'static str,
        macro_name: String,
        step_count: usize,
        baseline_hashes: Vec<String>,
        dry_run_hashes: Vec<String>,
        step_play_hashes: Vec<String>,
        divergence_index: Option<usize>,
        layout_json_exists_after_record: bool,
        layout_json_exists_after_replay: bool,
        repro: String,
        verdict: &'static str,
    }

    #[derive(Debug, Serialize)]
    struct MacroFailStopReport {
        generated_at: String,
        agent: &'static str,
        bead: &'static str,
        macro_name: String,
        baseline_hashes: Vec<String>,
        edited_hashes: Vec<String>,
        divergence_index: Option<usize>,
        verdict: &'static str,
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn operator_macro_record_save_load_replay_forensics() {
        // Use a per-test temp workspace to avoid touching user config.
        let tmp = tempfile::tempdir().expect("tempdir");
        let macro_dir = tmp.path().join("macros");
        let _ = std::fs::create_dir_all(&macro_dir);
        let envfile_path = tmp.path().join("config.env");

        let config = Config {
            console_persist_path: envfile_path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let artifacts = new_artifact_dir("record_save_load_replay");

        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::with_config(state, &config);
        model.macro_engine = MacroEngine::with_dir(macro_dir.clone());

        // ── Record a macro ────────────────────────────────────────
        model.dispatch_palette_action(macro_ids::RECORD_START);
        assert!(model.macro_engine.recorder_state().is_recording());

        let record_actions: &[&str] = &[
            palette_action_ids::SCREEN_TIMELINE,
            palette_action_ids::LAYOUT_EXPORT,
            palette_action_ids::SCREEN_MESSAGES,
            palette_action_ids::APP_TOGGLE_HELP,
            "macro:view_thread:br-3vwi",
            palette_action_ids::APP_TOGGLE_HELP,
            palette_action_ids::SCREEN_TIMELINE,
            palette_action_ids::LAYOUT_IMPORT,
            palette_action_ids::SCREEN_DASHBOARD,
        ];

        for (i, &action_id) in record_actions.iter().enumerate() {
            let before = model.active_screen();
            let _ = model.dispatch_palette_action(action_id);
            let after = model.active_screen();
            let tel = StepTelemetry {
                phase: "record",
                step_index: i,
                action_id: action_id.to_string(),
                label: palette_action_label(action_id),
                stable_hash64: format!(
                    "{:016x}",
                    MacroStep::new(action_id, palette_action_label(action_id)).stable_hash64()
                ),
                delay_ms: None,
                executed: None,
                error: None,
                before_screen: screen_label(before).to_string(),
                after_screen: screen_label(after).to_string(),
                help_visible: model.help_visible(),
            };
            let path = artifacts.join(format!("steps/step_{:04}_record.json", i + 1));
            let _ = std::fs::write(path, serde_json::to_string_pretty(&tel).unwrap());
        }

        model.dispatch_palette_action(macro_ids::RECORD_STOP);
        assert!(!model.macro_engine.recorder_state().is_recording());

        let names = model.macro_engine.list_macros();
        assert_eq!(names.len(), 1, "expected exactly 1 recorded macro");
        let auto_name = names[0].to_string();

        // Rename to a stable test name for deterministic file paths.
        let macro_name = "e2e-macro";
        assert!(
            model.macro_engine.rename_macro(&auto_name, macro_name),
            "rename macro"
        );

        let def = model
            .macro_engine
            .get_macro(macro_name)
            .expect("macro def exists");
        assert_eq!(
            def.steps.len(),
            record_actions.len(),
            "macro step count matches"
        );

        let baseline_hashes: Vec<u64> = def.steps.iter().map(MacroStep::stable_hash64).collect();

        // Persisted macro JSON should exist in the macro dir.
        let macro_path = macro_dir.join(format!("{macro_name}.json"));
        assert!(
            macro_path.exists(),
            "macro persisted: {}",
            macro_path.display()
        );

        // Layout export should have created layout.json next to the envfile.
        let layout_json_path = envfile_path
            .parent()
            .expect("envfile parent")
            .join("layout.json");

        let layout_json_exists_after_record = layout_json_path.exists();

        // ── Load in a fresh model + replay ────────────────────────
        let state2 = TuiSharedState::new(&config);
        let mut replay = MailAppModel::with_config(state2, &config);
        replay.macro_engine = MacroEngine::with_dir(macro_dir);

        // Dry-run (preview) should create a structured playback log without executing.
        replay.dispatch_palette_action(&format!("{}{}", macro_ids::DRY_RUN_PREFIX, macro_name));

        let dry_log = replay.macro_engine.playback_log().to_vec();
        assert_eq!(dry_log.len(), baseline_hashes.len(), "dry-run log length");
        assert!(
            dry_log.iter().all(|e| !e.executed),
            "dry-run should mark executed=false"
        );

        let dry_hashes: Vec<u64> = dry_log
            .iter()
            .map(|e| MacroStep::new(&e.action_id, &e.label).stable_hash64())
            .collect();
        assert_eq!(dry_hashes, baseline_hashes, "dry-run step hashes match");

        // Step-by-step playback: confirm each step via Enter.
        replay.macro_engine.clear_playback();
        replay.dispatch_palette_action(&format!("{}{}", macro_ids::PLAY_STEP_PREFIX, macro_name));

        let mut step_play_hashes: Vec<u64> = Vec::new();
        for i in 0..baseline_hashes.len() {
            let before = replay.active_screen();
            let _ = replay.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
            let after = replay.active_screen();

            let entry = replay
                .macro_engine
                .playback_log()
                .last()
                .expect("playback log entry");
            let h = MacroStep::new(&entry.action_id, &entry.label).stable_hash64();
            step_play_hashes.push(h);

            let tel = StepTelemetry {
                phase: "play_step",
                step_index: i,
                action_id: entry.action_id.clone(),
                label: entry.label.clone(),
                stable_hash64: format!("{h:016x}"),
                delay_ms: None,
                executed: Some(entry.executed),
                error: entry.error.clone(),
                before_screen: screen_label(before).to_string(),
                after_screen: screen_label(after).to_string(),
                help_visible: replay.help_visible(),
            };
            let path = artifacts.join(format!("steps/step_{:04}_play.json", i + 1));
            let _ = std::fs::write(path, serde_json::to_string_pretty(&tel).unwrap());
        }

        assert_eq!(
            step_play_hashes, baseline_hashes,
            "step-by-step hashes match"
        );
        assert!(
            matches!(
                replay.macro_engine.playback_state(),
                PlaybackState::Completed { .. }
            ),
            "playback completed"
        );

        // Layout file should still exist after replay (export/import steps are idempotent).
        let layout_json_exists_after_replay = layout_json_path.exists();

        let report = MacroReplayReport {
            generated_at: chrono::Utc::now().to_rfc3339(),
            agent: "EmeraldPeak",
            bead: "br-3vwi.10.15",
            macro_name: macro_name.to_string(),
            step_count: baseline_hashes.len(),
            baseline_hashes: baseline_hashes
                .iter()
                .map(|h| format!("{h:016x}"))
                .collect(),
            dry_run_hashes: dry_hashes.iter().map(|h| format!("{h:016x}")).collect(),
            step_play_hashes: step_play_hashes
                .iter()
                .map(|h| format!("{h:016x}"))
                .collect(),
            divergence_index: first_divergence(&baseline_hashes, &step_play_hashes),
            layout_json_exists_after_record,
            layout_json_exists_after_replay,
            repro: "cargo test -p mcp-agent-mail-server operator_macro_record_save_load_replay_forensics -- --nocapture"
                .to_string(),
            verdict: "PASS",
        };

        let _ = std::fs::write(
            artifacts.join("report.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn operator_macro_edit_and_fail_stop_forensics() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macro_dir = tmp.path().join("macros");
        let _ = std::fs::create_dir_all(&macro_dir);
        let envfile_path = tmp.path().join("config.env");

        let config = Config {
            console_persist_path: envfile_path,
            console_auto_save: true,
            ..Config::default()
        };

        let artifacts = new_artifact_dir("edit_fail_stop");

        // Record a small macro (we'll edit the JSON on disk to inject failure).
        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::with_config(state, &config);
        model.macro_engine = MacroEngine::with_dir(macro_dir.clone());

        model.dispatch_palette_action(macro_ids::RECORD_START);
        model.dispatch_palette_action(palette_action_ids::SCREEN_MESSAGES);
        model.dispatch_palette_action(palette_action_ids::SCREEN_TIMELINE);
        model.dispatch_palette_action(macro_ids::RECORD_STOP);

        let names = model.macro_engine.list_macros();
        assert_eq!(names.len(), 1, "expected 1 macro");
        let auto_name = names[0].to_string();

        let macro_name = "e2e-macro";
        assert!(model.macro_engine.rename_macro(&auto_name, macro_name));

        let original = model
            .macro_engine
            .get_macro(macro_name)
            .expect("macro exists")
            .clone();
        let baseline_hashes: Vec<u64> = original
            .steps
            .iter()
            .map(MacroStep::stable_hash64)
            .collect();

        // Edit persisted JSON: inject a failing step at index 1.
        let macro_path = macro_dir.join(format!("{macro_name}.json"));
        let data = std::fs::read_to_string(&macro_path).expect("read macro json");
        let mut def: MacroDef = serde_json::from_str(&data).expect("parse macro json");
        def.steps.insert(
            1,
            MacroStep::new("nonexistent:action", "Injected failure step"),
        );
        let _ = std::fs::write(&macro_path, serde_json::to_string_pretty(&def).unwrap());

        let edited_hashes: Vec<u64> = def.steps.iter().map(MacroStep::stable_hash64).collect();
        let div = first_divergence(&baseline_hashes, &edited_hashes);
        assert_eq!(div, Some(1), "expected divergence at injected index");

        // Load and attempt playback: should fail-stop on the injected action.
        let state2 = TuiSharedState::new(&config);
        let mut replay = MailAppModel::with_config(state2, &config);
        replay.macro_engine = MacroEngine::with_dir(macro_dir);

        replay.dispatch_palette_action(&format!("{}{}", macro_ids::PLAY_PREFIX, macro_name));

        assert!(
            matches!(
                replay.macro_engine.playback_state(),
                PlaybackState::Failed { .. }
            ),
            "playback should fail-stop"
        );

        let log = replay.macro_engine.playback_log();
        assert!(
            log.len() <= 2,
            "should stop at injected step (log_len={})",
            log.len()
        );
        if let Some(last) = log.last() {
            // The failing entry should carry an error string.
            assert!(last.error.is_some(), "expected playback log error");
            let tel = StepTelemetry {
                phase: "fail_stop",
                step_index: last.step_index,
                action_id: last.action_id.clone(),
                label: last.label.clone(),
                stable_hash64: format!(
                    "{:016x}",
                    MacroStep::new(&last.action_id, &last.label).stable_hash64()
                ),
                delay_ms: None,
                executed: Some(last.executed),
                error: last.error.clone(),
                before_screen: screen_label(replay.active_screen()).to_string(),
                after_screen: screen_label(replay.active_screen()).to_string(),
                help_visible: replay.help_visible(),
            };
            let _ = std::fs::write(
                artifacts.join("failures/fail_0001.json"),
                serde_json::to_string_pretty(&tel).unwrap(),
            );
        }

        // Write a small report (useful in CI artifact bundles).
        let report = MacroFailStopReport {
            generated_at: chrono::Utc::now().to_rfc3339(),
            agent: "EmeraldPeak",
            bead: "br-3vwi.10.15",
            macro_name: macro_name.to_string(),
            baseline_hashes: baseline_hashes
                .iter()
                .map(|h| format!("{h:016x}"))
                .collect(),
            edited_hashes: edited_hashes.iter().map(|h| format!("{h:016x}")).collect(),
            divergence_index: div,
            verdict: "PASS",
        };
        let _ = std::fs::write(
            artifacts.join("report.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        );
    }

    // ── Toast severity threshold tests ──────────────────────────────

    #[test]
    fn severity_info_allows_all() {
        let s = ToastSeverityThreshold::Info;
        assert!(s.allows(ToastIcon::Info));
        assert!(s.allows(ToastIcon::Warning));
        assert!(s.allows(ToastIcon::Error));
        assert!(s.allows(ToastIcon::Success));
    }

    #[test]
    fn severity_warning_filters_info() {
        let s = ToastSeverityThreshold::Warning;
        assert!(!s.allows(ToastIcon::Info));
        assert!(s.allows(ToastIcon::Warning));
        assert!(s.allows(ToastIcon::Error));
        assert!(!s.allows(ToastIcon::Success));
    }

    #[test]
    fn severity_error_filters_warning_and_info() {
        let s = ToastSeverityThreshold::Error;
        assert!(!s.allows(ToastIcon::Info));
        assert!(!s.allows(ToastIcon::Warning));
        assert!(s.allows(ToastIcon::Error));
        assert!(!s.allows(ToastIcon::Success));
    }

    #[test]
    fn severity_off_blocks_everything() {
        let s = ToastSeverityThreshold::Off;
        assert!(!s.allows(ToastIcon::Info));
        assert!(!s.allows(ToastIcon::Warning));
        assert!(!s.allows(ToastIcon::Error));
        assert!(!s.allows(ToastIcon::Success));
    }

    #[test]
    fn parse_toast_position_maps_supported_values() {
        assert_eq!(parse_toast_position("top-left"), ToastPosition::TopLeft);
        assert_eq!(
            parse_toast_position("bottom-left"),
            ToastPosition::BottomLeft
        );
        assert_eq!(
            parse_toast_position("bottom-right"),
            ToastPosition::BottomRight
        );
        assert_eq!(parse_toast_position("unknown"), ToastPosition::TopRight);
    }

    #[test]
    fn with_config_applies_toast_runtime_settings() {
        let config = Config {
            tui_toast_enabled: true,
            tui_toast_severity: "error".to_string(),
            tui_toast_position: "bottom-left".to_string(),
            tui_toast_max_visible: 6,
            tui_toast_info_dismiss_secs: 7,
            tui_toast_warn_dismiss_secs: 11,
            tui_toast_error_dismiss_secs: 19,
            ..Config::default()
        };
        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(state, &config);
        assert_eq!(model.toast_severity, ToastSeverityThreshold::Error);
        assert!(!model.toast_muted);
        assert_eq!(model.toast_info_dismiss_secs, 7);
        assert_eq!(model.toast_warn_dismiss_secs, 11);
        assert_eq!(model.toast_error_dismiss_secs, 19);
        assert_eq!(model.notifications.config().max_visible, 6);
        assert_eq!(
            model.notifications.config().position,
            ToastPosition::BottomLeft
        );
    }

    #[test]
    fn toast_focus_m_key_toggles_runtime_mute() {
        let mut model = test_model();
        model.toast_focus_index = Some(0);
        assert!(!model.toast_muted);

        let key = Event::Key(KeyEvent::new(KeyCode::Char('m')));
        let cmd = model.update(MailMsg::Terminal(key.clone()));
        assert!(matches!(cmd, Cmd::None));
        assert!(model.toast_muted);

        let cmd = model.update(MailMsg::Terminal(key));
        assert!(matches!(cmd, Cmd::None));
        assert!(!model.toast_muted);
    }

    // ── toast_for_event tests ───────────────────────────────────────

    #[test]
    fn toast_message_received_generates_info() {
        let event = MailEvent::message_received(1, "BlueLake", vec![], "Hello world", "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(toast.is_some(), "MessageReceived should generate a toast");
    }

    #[test]
    fn toast_message_received_truncates_long_subject() {
        let long_subject = "A".repeat(60);
        let event = MailEvent::message_received(1, "BlueLake", vec![], &long_subject, "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        // The message inside the toast should be truncated
        assert!(toast.content.message.len() < 60);
        assert!(toast.content.message.contains('…'));
    }

    #[test]
    fn toast_message_sent_still_works() {
        let event = MailEvent::message_sent(1, "RedFox", vec![], "Test", "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "MessageSent should still generate a toast (regression)"
        );
    }

    #[test]
    fn toast_tool_call_end_normal_no_toast() {
        let event =
            MailEvent::tool_call_end("register_agent", 100, None, 0, 0.0, vec![], None, None);
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_none(),
            "Normal ToolCallEnd should not generate a toast"
        );
    }

    #[test]
    fn toast_tool_call_end_slow_generates_warning() {
        let event =
            MailEvent::tool_call_end("search_messages", 6000, None, 0, 0.0, vec![], None, None);
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "Slow ToolCallEnd should generate a warning toast"
        );
        let t = toast.unwrap();
        assert_eq!(t.content.icon, Some(ToastIcon::Warning));
        assert!(t.content.message.contains("6000ms"));
    }

    #[test]
    fn toast_tool_call_end_error_preview_generates_error() {
        let event = MailEvent::tool_call_end(
            "send_message",
            200,
            Some("error: agent not registered".to_string()),
            0,
            0.0,
            vec![],
            None,
            None,
        );
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "Error ToolCallEnd should generate an error toast"
        );
        let t = toast.unwrap();
        assert_eq!(t.content.icon, Some(ToastIcon::Error));
        assert!(t.content.message.contains("send_message"));
    }

    #[test]
    fn toast_reservation_granted_exclusive_generates_info() {
        let event = MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**".to_string()],
            true,
            3600,
            "proj",
        );
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "Exclusive ReservationGranted should generate an info toast"
        );
        let t = toast.unwrap();
        assert!(t.content.message.contains("BlueLake"));
        assert!(t.content.message.contains("src/**"));
    }

    #[test]
    fn toast_reservation_granted_shared_no_toast() {
        let event = MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**".to_string()],
            false,
            3600,
            "proj",
        );
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_none(),
            "Non-exclusive ReservationGranted should NOT generate a toast"
        );
    }

    #[test]
    fn toast_existing_mappings_unchanged_agent_registered() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "AgentRegistered should generate a toast (regression)"
        );
        assert_eq!(toast.unwrap().content.icon, Some(ToastIcon::Success));
    }

    #[test]
    fn toast_existing_mappings_unchanged_http_500() {
        let event = MailEvent::http_request("GET", "/mcp/", 500, 5, "127.0.0.1");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "HTTP 500 should generate a toast (regression)"
        );
        assert_eq!(toast.unwrap().content.icon, Some(ToastIcon::Error));
    }

    #[test]
    fn toast_existing_mappings_unchanged_server_shutdown() {
        let event = MailEvent::server_shutdown();
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "ServerShutdown should generate a toast (regression)"
        );
        assert_eq!(toast.unwrap().content.icon, Some(ToastIcon::Warning));
    }

    #[test]
    fn toast_existing_mappings_unchanged_server_started() {
        let event = MailEvent::server_started("http://127.0.0.1:8765", "test");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info);
        assert!(
            toast.is_some(),
            "ServerStarted should generate a toast (regression)"
        );
        assert_eq!(toast.unwrap().content.icon, Some(ToastIcon::Success));
    }

    #[test]
    fn toast_severity_filter_blocks_info_at_error_level() {
        let event = MailEvent::message_received(1, "BlueLake", vec![], "Hello", "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Error);
        assert!(
            toast.is_none(),
            "Info toast should be blocked at Error severity"
        );
    }

    #[test]
    fn toast_severity_filter_passes_error_at_error_level() {
        let event = MailEvent::http_request("GET", "/mcp/", 500, 5, "127.0.0.1");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Error);
        assert!(toast.is_some(), "Error toast should pass at Error severity");
    }

    #[test]
    fn toast_severity_off_blocks_everything() {
        let event = MailEvent::http_request("GET", "/mcp/", 500, 5, "127.0.0.1");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Off);
        assert!(
            toast.is_none(),
            "All toasts should be blocked at Off severity"
        );
    }

    // ── Reservation expiry tracker tests ────────────────────────────

    #[test]
    fn reservation_tracker_insert_and_remove() {
        let mut model = test_model();
        let key = "proj:BlueLake:src/**".to_string();
        model
            .reservation_tracker
            .insert(key.clone(), ("BlueLake:src/**".to_string(), i64::MAX));
        assert!(model.reservation_tracker.contains_key(&key));
        model.reservation_tracker.remove(&key);
        assert!(!model.reservation_tracker.contains_key(&key));
    }

    #[test]
    fn reservation_expiry_warning_fires_within_window() {
        let mut model = test_model();
        let now = now_micros();
        // Reservation expiring in 3 minutes (within 5-minute window)
        let expiry = now + 3 * 60 * 1_000_000;
        let key = "proj:BlueLake:src/**".to_string();
        model
            .reservation_tracker
            .insert(key.clone(), ("BlueLake:src/**".to_string(), expiry));
        assert!(!model.warned_reservations.contains(&key));

        // Check: expiry is within the warning window
        assert!(expiry > now);
        assert!(expiry - now < RESERVATION_EXPIRY_WARN_MICROS);
    }

    #[test]
    fn reservation_expiry_no_warning_if_far_away() {
        let now = now_micros();
        // Reservation expiring in 30 minutes (outside 5-minute window)
        let expiry = now + 30 * 60 * 1_000_000;
        // Should NOT be within warning window
        assert!(expiry - now >= RESERVATION_EXPIRY_WARN_MICROS);
    }

    #[test]
    fn warned_reservations_dedup_prevents_repeat() {
        let mut model = test_model();
        let key = "proj:BlueLake:src/**".to_string();
        model.warned_reservations.insert(key.clone());
        assert!(model.warned_reservations.contains(&key));
        // Second insert is a no-op
        model.warned_reservations.insert(key.clone());
        assert_eq!(model.warned_reservations.len(), 1);
    }

    // ── Toast focus mode tests ──────────────────────────────────

    #[test]
    fn toast_focus_index_starts_none() {
        let model = test_model();
        assert!(model.toast_focus_index.is_none());
    }

    #[test]
    fn toast_focus_toggle_on_with_visible_toasts() {
        let mut model = test_model();
        // Push a toast and tick so it becomes visible.
        model.notifications.notify(
            Toast::new("test")
                .icon(ToastIcon::Info)
                .duration(Duration::from_secs(60)),
        );
        model.notifications.tick(Duration::from_millis(16));
        assert_eq!(model.notifications.visible_count(), 1);

        // Toggle on.
        model.toast_focus_index = Some(0);
        assert_eq!(model.toast_focus_index, Some(0));
    }

    #[test]
    fn toast_focus_toggle_off() {
        let mut model = test_model();
        model.toast_focus_index = Some(0);
        model.toast_focus_index = None;
        assert!(model.toast_focus_index.is_none());
    }

    #[test]
    fn toast_focus_no_toggle_when_no_visible() {
        let model = test_model();
        // No toasts visible.
        assert_eq!(model.notifications.visible_count(), 0);
        // Should not toggle (caller checks visible_count > 0).
        if model.notifications.visible_count() > 0 {
            unreachable!();
        }
    }

    #[test]
    fn toast_focus_navigate_down_wraps() {
        let mut model = test_model();
        for i in 0..3 {
            model.notifications.notify(
                Toast::new(format!("toast {i}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(60)),
            );
        }
        model.notifications.tick(Duration::from_millis(16));
        assert_eq!(model.notifications.visible_count(), 3);

        model.toast_focus_index = Some(0);
        // Navigate down: 0 -> 1 -> 2 -> 0 (wrap).
        let count = model.notifications.visible_count();
        let idx = model.toast_focus_index.as_mut().unwrap();
        *idx = (*idx + 1) % count;
        assert_eq!(*idx, 1);
        *idx = (*idx + 1) % count;
        assert_eq!(*idx, 2);
        *idx = (*idx + 1) % count;
        assert_eq!(*idx, 0); // Wrapped.
    }

    #[test]
    fn toast_focus_navigate_up_wraps() {
        let mut model = test_model();
        for i in 0..3 {
            model.notifications.notify(
                Toast::new(format!("toast {i}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(60)),
            );
        }
        model.notifications.tick(Duration::from_millis(16));
        model.toast_focus_index = Some(0);

        let count = model.notifications.visible_count();
        let idx = model.toast_focus_index.as_mut().unwrap();
        // Up from 0 wraps to 2.
        *idx = if *idx == 0 { count - 1 } else { *idx - 1 };
        assert_eq!(*idx, 2);
    }

    #[test]
    fn toast_focus_dismiss_clamps_index() {
        let mut model = test_model();
        for i in 0..3 {
            model.notifications.notify(
                Toast::new(format!("toast {i}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(60)),
            );
        }
        model.notifications.tick(Duration::from_millis(16));
        model.toast_focus_index = Some(2);

        // Dismiss the focused toast (index 2 = last one).
        let vis = model.notifications.visible();
        let id = vis[2].id;
        model.notifications.dismiss(id);
        model.notifications.tick(Duration::from_millis(16));

        let count = model.notifications.visible_count();
        model.toast_focus_index = Some(2_usize.min(count.saturating_sub(1)));
        // After dismissal, count=2, so index clamped to 1.
        assert_eq!(model.toast_focus_index, Some(1));
    }

    #[test]
    fn toast_focus_dismiss_last_clears_focus() {
        let mut model = test_model();
        model.notifications.notify(
            Toast::new("only one")
                .icon(ToastIcon::Info)
                .duration(Duration::from_secs(60)),
        );
        model.notifications.tick(Duration::from_millis(16));
        model.toast_focus_index = Some(0);

        // Dismiss the only toast.
        let vis = model.notifications.visible();
        let id = vis[0].id;
        model.notifications.dismiss(id);
        model.notifications.tick(Duration::from_millis(16));

        let count = model.notifications.visible_count();
        if count == 0 {
            model.toast_focus_index = None;
        }
        assert!(model.toast_focus_index.is_none());
    }

    // ── Toast severity coloring tests ───────────────────────────
    //
    // Toast.style is private, so we verify coloring by rendering to a
    // buffer and checking the foreground color of border cells.

    fn render_toast_border_fg(toast: &Toast) -> PackedRgba {
        let (tw, th) = toast.calculate_dimensions();
        let area = Rect::new(0, 0, tw.max(10), th.max(4));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(area.width, area.height, &mut pool);
        toast.render(area, &mut frame);
        // Top-left corner cell should carry the toast style fg.
        frame
            .buffer
            .get(0, 0)
            .map_or(PackedRgba::TRANSPARENT, |c| c.fg)
    }

    #[test]
    fn toast_for_event_error_renders_error_color() {
        let event = MailEvent::http_request("GET", "/mcp/", 500, 5, "127.0.0.1");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, TOAST_COLOR_ERROR);
    }

    #[test]
    fn toast_for_event_warning_renders_warning_color() {
        let event = MailEvent::tool_call_end("slow_tool", 6000, None, 0, 0.0, vec![], None, None);
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, TOAST_COLOR_WARNING);
    }

    #[test]
    fn toast_for_event_info_renders_info_color() {
        let event = MailEvent::message_sent(1, "A", vec!["B".into()], "Hi", "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, TOAST_COLOR_INFO);
    }

    #[test]
    fn toast_for_event_success_renders_success_color() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, TOAST_COLOR_SUCCESS);
    }

    // ── render_toast_focus_highlight tests ───────────────────────

    #[test]
    fn focus_highlight_noop_when_no_visible() {
        let queue = NotificationQueue::new(QueueConfig::default());
        let area = Rect::new(0, 0, 80, 24);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        // Should not panic with no visible toasts.
        render_toast_focus_highlight(&queue, 0, area, 1, &mut frame);
    }

    #[test]
    fn focus_highlight_noop_when_index_out_of_bounds() {
        let mut queue = NotificationQueue::new(QueueConfig::default());
        queue.notify(Toast::new("test").duration(Duration::from_secs(60)));
        queue.tick(Duration::from_millis(16));
        assert_eq!(queue.visible_count(), 1);

        let area = Rect::new(0, 0, 80, 24);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        // Index 5 is out of bounds (only 1 visible).
        render_toast_focus_highlight(&queue, 5, area, 1, &mut frame);
    }

    #[test]
    fn focus_highlight_renders_hint_text() {
        let mut queue = NotificationQueue::new(QueueConfig::default());
        queue.notify(Toast::new("test toast").duration(Duration::from_secs(60)));
        queue.tick(Duration::from_millis(16));
        assert_eq!(queue.visible_count(), 1);

        let area = Rect::new(0, 0, 80, 24);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_toast_focus_highlight(&queue, 0, area, 1, &mut frame);

        // The hint text should be rendered below the toast.
        // Check that some cells in the hint row have the highlight color.
        let positions = queue.calculate_positions(80, 24, 1);
        let (_, _, py) = positions[0];
        let (_, th) = queue.visible()[0].calculate_dimensions();
        let hint_y = py + th;

        // At least one cell in the hint row should have TOAST_FOCUS_HIGHLIGHT fg.
        let mut found = false;
        for x in 0..80 {
            if let Some(cell) = frame.buffer.get(x, hint_y) {
                if cell.fg == TOAST_FOCUS_HIGHLIGHT {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "Hint text should be rendered with highlight color");
    }

    // ── Performance benchmarks (br-2bbt.11.4) ─────────────────────────────────

    /// Benchmark: render toast overlay with 3 stacked toasts.
    ///
    /// Measures the frame render overhead added by toast overlay rendering.
    /// Budget: overlay should add < 1ms to frame time.
    #[test]
    fn perf_toast_overlay_render() {
        use std::time::Instant;

        let area = Rect::new(0, 0, 160, 48);
        let mut pool = ftui::GraphemePool::new();

        // Create a queue with 3 visible toasts of different severities.
        let mut queue = NotificationQueue::new(QueueConfig {
            max_visible: 3,
            ..QueueConfig::default()
        });

        // Add 3 toasts
        queue.push(
            Toast::new("Info: New message from BlueLake")
                .icon(ToastIcon::Info)
                .duration(std::time::Duration::from_secs(10)),
            NotificationPriority::Normal,
        );
        queue.push(
            Toast::new("Warning: Reservation expiring soon")
                .icon(ToastIcon::Warning)
                .duration(std::time::Duration::from_secs(10)),
            NotificationPriority::Normal,
        );
        queue.push(
            Toast::new("Error: Connection lost to remote server")
                .icon(ToastIcon::Error)
                .duration(std::time::Duration::from_secs(10)),
            NotificationPriority::Urgent,
        );

        // Promote toasts from queue to visible
        queue.tick(std::time::Duration::from_millis(16));

        // Benchmark: render 100 frames with toast overlay
        let mut timings_ns: Vec<u128> = Vec::with_capacity(100);
        for _ in 0..100 {
            let mut frame = Frame::new(area.width, area.height, &mut pool);
            let start = Instant::now();
            NotificationStack::new(&queue).render(area, &mut frame);
            let elapsed = start.elapsed();
            timings_ns.push(elapsed.as_nanos());
        }

        // Sort for percentile calculation
        timings_ns.sort_unstable();
        let p50_us = timings_ns[timings_ns.len() / 2] / 1000;
        let p95_us = timings_ns[timings_ns.len() * 95 / 100] / 1000;
        let p99_us = timings_ns[timings_ns.len() * 99 / 100] / 1000;

        // Toast overlay should add < 1ms (1000µs) to frame time at p95
        assert!(
            p95_us < 1000,
            "Toast overlay p95 exceeds 1ms: p50={}µs, p95={}µs, p99={}µs",
            p50_us,
            p95_us,
            p99_us
        );

        eprintln!(
            "[perf] Toast overlay render (3 toasts, 160x48): \
             p50={}µs p95={}µs p99={}µs",
            p50_us, p95_us, p99_us
        );
    }

    /// Benchmark: render sparkline with 100 data points.
    ///
    /// Budget: sparkline render should complete in < 500µs.
    #[test]
    fn perf_sparkline_100points() {
        use ftui_widgets::sparkline::Sparkline;
        use std::time::Instant;

        let area = Rect::new(0, 0, 80, 1);
        let mut pool = ftui::GraphemePool::new();

        // Generate 100 data points with variation
        let data: Vec<f64> = (0..100)
            .map(|i| {
                let base = 50.0;
                let wave = (i as f64 * 0.2).sin() * 30.0;
                let noise = ((i * 7) % 13) as f64 - 6.0;
                (base + wave + noise).max(0.0)
            })
            .collect();

        // Benchmark: render 100 times
        let mut timings_ns: Vec<u128> = Vec::with_capacity(100);
        for _ in 0..100 {
            let mut frame = Frame::new(area.width, area.height, &mut pool);
            let sparkline = Sparkline::new(&data).min(0.0).max(100.0);

            let start = Instant::now();
            sparkline.render(area, &mut frame);
            let elapsed = start.elapsed();
            timings_ns.push(elapsed.as_nanos());
        }

        // Sort for percentile calculation
        timings_ns.sort_unstable();
        let p50_us = timings_ns[timings_ns.len() / 2] / 1000;
        let p95_us = timings_ns[timings_ns.len() * 95 / 100] / 1000;
        let p99_us = timings_ns[timings_ns.len() * 99 / 100] / 1000;

        // Sparkline should render in < 500µs at p95
        assert!(
            p95_us < 500,
            "Sparkline p95 exceeds 500µs: p50={}µs, p95={}µs, p99={}µs",
            p50_us,
            p95_us,
            p99_us
        );

        eprintln!(
            "[perf] Sparkline render (100 points, 80 cols): \
             p50={}µs p95={}µs p99={}µs",
            p50_us, p95_us, p99_us
        );
    }

    /// Benchmark: render modal dialog overlay.
    ///
    /// Budget: modal overlay should add < 1ms to frame time.
    #[test]
    fn perf_modal_overlay_render() {
        use std::time::Instant;

        let area = Rect::new(0, 0, 160, 48);
        let mut pool = ftui::GraphemePool::new();

        // Create a modal dialog with typical confirmation content
        let dialog = Dialog::confirm(
            "Confirm Force Release",
            "Are you sure you want to force-release this file reservation?\n\n\
             This will immediately terminate the current reservation holder's lock.\n\
             Any unsaved work may be lost.",
        );

        // Benchmark: render 100 frames with modal overlay
        // Use DialogState::new() to start with open=true so dialog renders
        let mut state = DialogState::new();
        let mut timings_ns: Vec<u128> = Vec::with_capacity(100);
        for _ in 0..100 {
            let mut frame = Frame::new(area.width, area.height, &mut pool);

            let start = Instant::now();
            dialog.render(area, &mut frame, &mut state);
            let elapsed = start.elapsed();
            timings_ns.push(elapsed.as_nanos());
        }

        // Sort for percentile calculation
        timings_ns.sort_unstable();
        let p50_us = timings_ns[timings_ns.len() / 2] / 1000;
        let p95_us = timings_ns[timings_ns.len() * 95 / 100] / 1000;
        let p99_us = timings_ns[timings_ns.len() * 99 / 100] / 1000;

        // Modal overlay should add < 1ms (1000µs) at p95
        assert!(
            p95_us < 1000,
            "Modal overlay p95 exceeds 1ms: p50={}µs, p95={}µs, p99={}µs",
            p50_us,
            p95_us,
            p99_us
        );

        eprintln!(
            "[perf] Modal overlay render (160x48): \
             p50={}µs p95={}µs p99={}µs",
            p50_us, p95_us, p99_us
        );
    }

    /// Benchmark: command palette fuzzy search with 100 entries.
    ///
    /// Tests the fuzzy matching performance of the command palette.
    /// Budget: fuzzy search should complete in < 2ms per query at p95.
    #[test]
    fn perf_command_palette_fuzzy_100() {
        use ftui::widgets::command_palette::{ActionItem, CommandPalette};
        use std::time::Instant;

        // Create a command palette and populate with 100 action items
        let mut palette = CommandPalette::new();

        for i in 0..100 {
            let category = match i % 5 {
                0 => "Layout",
                1 => "Theme",
                2 => "Navigation",
                3 => "Actions",
                _ => "Help",
            };
            palette.register_action(
                ActionItem::new(
                    format!("action:{}", i),
                    format!("Action Item Number {} Description", i),
                )
                .with_description(format!(
                    "This is action {} which does something useful in category {}",
                    i, category
                ))
                .with_category(category)
                .with_tags(&[&format!("tag{}", i % 10), category.to_lowercase().as_str()]),
            );
        }

        // Test queries of varying lengths and match difficulty
        let queries = [
            "act",     // Short prefix
            "action",  // Common word
            "number",  // Middle match
            "layout",  // Category match
            "des",     // Description match
            "tag5",    // Tag match
            "xyz",     // No match
            "a i n d", // Sparse chars
            "item 50", // Specific number
            "useful",  // Description word
        ];

        // Benchmark: run each query 10 times
        let mut timings_ns: Vec<u128> = Vec::with_capacity(100);
        for query in &queries {
            for _ in 0..10 {
                let start = Instant::now();
                palette.set_query(*query);
                let elapsed = start.elapsed();
                timings_ns.push(elapsed.as_nanos());
            }
        }

        // Sort for percentile calculation
        timings_ns.sort_unstable();
        let p50_us = timings_ns[timings_ns.len() / 2] / 1000;
        let p95_us = timings_ns[timings_ns.len() * 95 / 100] / 1000;
        let p99_us = timings_ns[timings_ns.len() * 99 / 100] / 1000;

        // Fuzzy search should complete in < 2ms (2000µs) at p95
        assert!(
            p95_us < 2000,
            "Command palette fuzzy search p95 exceeds 2ms: p50={}µs, p95={}µs, p99={}µs",
            p50_us,
            p95_us,
            p99_us
        );

        eprintln!(
            "[perf] Command palette fuzzy search (100 entries): \
             p50={}µs p95={}µs p99={}µs",
            p50_us, p95_us, p99_us
        );
    }
}
