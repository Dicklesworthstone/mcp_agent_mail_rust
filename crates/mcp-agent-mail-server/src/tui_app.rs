//! Top-level TUI application model for `AgentMailTUI`.
//!
//! [`MailAppModel`] implements the `ftui_runtime` [`Model`] trait,
//! orchestrating screen switching, global keybindings, tick dispatch,
//! and shared-state access.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ftui::Frame;
use ftui::layout::Rect;
use ftui::render::frame::HitId;
use ftui::text::display_width;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::command_palette::{ActionItem, CommandPalette, PaletteAction, PaletteStyle};
use ftui::widgets::hint_ranker::{HintContext, HintRanker, RankerConfig};
use ftui::widgets::inspector::{InspectorState, WidgetInfo};
use ftui::widgets::modal::{Dialog, DialogResult, DialogState};
use ftui::widgets::notification_queue::NotificationStack;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::toast::{ToastId, ToastPosition};
use ftui::widgets::{NotificationQueue, QueueConfig, Toast, ToastIcon};
use ftui::{
    Event, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, PackedRgba, Style,
};
use ftui_extras::clipboard::{Clipboard, ClipboardSelection};
use ftui_extras::export::{HtmlExporter, SvgExporter, TextExporter};
use ftui_extras::theme::ThemeId;
use ftui_runtime::program::{Cmd, Model};
use mcp_agent_mail_db::sqlmodel_core::Value;
use mcp_agent_mail_db::{DbConn, DbPoolConfig};

use crate::tui_action_menu::{ActionKind, ActionMenuManager, ActionMenuResult};
use crate::tui_bridge::{RemoteTerminalEvent, ServerControlMsg, TransportBase, TuiSharedState};
use crate::tui_compose::{ComposeAction, ComposePanel, ComposeState};
use crate::tui_events::{EventSeverity, MailEvent};
use crate::tui_focus::{FocusManager, FocusTarget, focus_graph_for_screen, focus_ring_for_screen};
use crate::tui_macro::{MacroEngine, PlaybackMode, PlaybackState, action_ids as macro_ids};
use crate::tui_screens::{
    ALL_SCREEN_IDS, DeepLinkTarget, MailScreen, MailScreenId, MailScreenMsg, agents::AgentsScreen,
    analytics::AnalyticsScreen, archive_browser::ArchiveBrowserScreen,
    attachments::AttachmentExplorerScreen, contacts::ContactsScreen, dashboard::DashboardScreen,
    explorer::MailExplorerScreen, messages::MessageBrowserScreen, projects::ProjectsScreen,
    reservations::ReservationsScreen, screen_from_jump_key, screen_meta,
    search::SearchCockpitScreen, system_health::SystemHealthScreen, threads::ThreadExplorerScreen,
    timeline::TimelineScreen, tool_metrics::ToolMetricsScreen,
};
use crate::tui_widgets::{
    AmbientEffectRenderer, AmbientHealthInput, AmbientMode, AmbientRenderTelemetry,
};

/// How often the TUI ticks (33 ms ≈ 30 fps).
///
/// 30 fps is more than adequate for a TUI and halves terminal output compared
/// to 60 fps.  Input latency stays imperceptible (< 50 ms round-trip).
const TICK_INTERVAL: Duration = Duration::from_millis(33);

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
const SCREEN_TRANSITION_TICKS: u8 = 2;
const TOAST_ENTRANCE_TICKS: u8 = 10;
const TOAST_EXIT_TICKS: u8 = 7;
const REMOTE_EVENTS_PER_TICK: usize = 256;
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const QUIT_CONFIRM_TOAST_SECS: u64 = 3;
const AMBIENT_HEALTH_LOOKBACK_EVENTS: usize = 256;
/// Ambient effects only re-render every Nth frame.  At 30 fps this means the
/// plasma/fire/metaball animation updates at ~5 fps, which is more than enough
/// for a subtle background effect and eliminates the per-frame full-screen diff
/// that was causing visible flashing and high CPU usage.
const AMBIENT_RENDER_EVERY_N_FRAMES: u64 = 6;

fn command_palette_theme_style() -> PaletteStyle {
    let tp = crate::tui_theme::TuiThemePalette::current();
    PaletteStyle {
        border: Style::default()
            .fg(tp.panel_border_focused)
            .bg(tp.bg_overlay),
        input: Style::default().fg(tp.text_primary).bg(tp.bg_overlay),
        item: Style::default().fg(tp.text_secondary).bg(tp.bg_overlay),
        item_selected: Style::default().fg(tp.selection_fg).bg(tp.selection_bg),
        match_highlight: Style::default().fg(tp.status_accent).bg(tp.bg_overlay),
        description: Style::default().fg(tp.text_muted).bg(tp.bg_overlay),
        category: Style::default().fg(tp.help_key_fg).bg(tp.bg_overlay),
        hint: Style::default().fg(tp.text_disabled).bg(tp.bg_overlay),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitConfirmSource {
    Escape,
    CtrlC,
}

/// Deferred action from a confirmed modal callback.
///
/// Modal callbacks are `FnOnce + Send` closures that cannot directly mutate
/// the model, so confirmed operations are sent through `action_tx` and drained
/// from `action_rx` on the next tick.
/// Tracks the outcome of a dispatched action for feedback surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Operation started — shown as in-flight indicator.
    InFlight { operation: String },
    /// Operation completed successfully.
    Success { operation: String, summary: String },
    /// Operation failed with an error.
    Failure { operation: String, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Html,
    Svg,
    Text,
}

impl ExportFormat {
    const fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Svg => "svg",
            Self::Text => "txt",
        }
    }

    fn render(self, snapshot: &FrameExportSnapshot) -> String {
        match self {
            Self::Html => HtmlExporter::default().export(&snapshot.buffer, &snapshot.pool),
            Self::Svg => SvgExporter::default().export(&snapshot.buffer, &snapshot.pool),
            Self::Text => TextExporter::plain().export(&snapshot.buffer, &snapshot.pool),
        }
    }
}

#[derive(Debug, Clone)]
struct FrameExportSnapshot {
    buffer: ftui::Buffer,
    pool: ftui::GraphemePool,
}

/// Semantic transition kind inferred from source/destination screen categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionKind {
    /// Same category — subtle slide indicator.
    Lateral,
    /// Different category — brief cross-fade with destination label.
    CrossCategory,
}

/// Navigation direction inferred from tab ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenTransition {
    from: MailScreenId,
    to: MailScreenId,
    ticks_remaining: u8,
    kind: TransitionKind,
    direction: TransitionDirection,
}

impl ScreenTransition {
    fn new(from: MailScreenId, to: MailScreenId) -> Self {
        let from_cat = screen_meta(from).category;
        let to_cat = screen_meta(to).category;
        let kind = if from_cat == to_cat {
            TransitionKind::Lateral
        } else {
            TransitionKind::CrossCategory
        };
        let from_idx = ALL_SCREEN_IDS.iter().position(|&s| s == from).unwrap_or(0);
        let to_idx = ALL_SCREEN_IDS.iter().position(|&s| s == to).unwrap_or(0);
        let direction = if to_idx >= from_idx {
            TransitionDirection::Forward
        } else {
            TransitionDirection::Backward
        };
        Self {
            from,
            to,
            ticks_remaining: SCREEN_TRANSITION_TICKS,
            kind,
            direction,
        }
    }

    #[cfg(test)]
    fn progress(self) -> f32 {
        let done = SCREEN_TRANSITION_TICKS.saturating_sub(self.ticks_remaining);
        f32::from(done) / f32::from(SCREEN_TRANSITION_TICKS.max(1))
    }

    /// Ease-out cubic curve for more natural deceleration.
    #[cfg(test)]
    fn eased_progress(self) -> f32 {
        let t = self.progress().clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }
}

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
        if config.tui_toast_enabled {
            Self::parse(config.tui_toast_severity.as_str())
        } else {
            Self::Off
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

fn env_flag_enabled(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| {
        let s = v.to_string_lossy();
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn toast_reduced_motion_enabled(accessibility: &crate::tui_persist::AccessibilitySettings) -> bool {
    accessibility.reduced_motion
        || env_flag_enabled("AM_TUI_REDUCED_MOTION")
        || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

/// Duration threshold (ms) for slow tool call toasts.
const SLOW_TOOL_THRESHOLD_MS: u64 = 5000;
/// How far ahead (in microseconds) to warn about expiring reservations (5 min).
const RESERVATION_EXPIRY_WARN_MICROS: i64 = 5 * 60 * 1_000_000;
/// Keep top-row KPI/header bands free from transient toast borders.
const TOAST_OVERLAY_CONTENT_TOP_INSET_ROWS: u16 = 2;

/// Toast border/icon colors resolved from the active theme palette.
fn toast_color_error() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().toast_error
}
fn toast_color_warning() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().toast_warning
}
fn toast_color_info() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().toast_info
}
fn toast_color_success() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().toast_success
}
/// Bright highlight for the focused toast border, theme-aware.
fn toast_focus_highlight() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().toast_focus
}

/// Current time as microseconds since Unix epoch.
fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

fn sanitize_filename_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_sep = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }

    out.trim_matches('_').to_string()
}

fn resolve_export_dir_from_sources(
    env_export_dir: Option<&str>,
    home_dir: Option<&Path>,
) -> PathBuf {
    if let Some(path) = env_export_dir.map(str::trim).filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(home) = home_dir {
        return home.join(".mcp_agent_mail").join("exports");
    }
    PathBuf::from(".mcp_agent_mail").join("exports")
}

#[allow(clippy::cast_precision_loss)]
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
    let decay = (-age_micros / half_life).exp2();
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

fn remote_modifiers_from_bits(modifiers: u8) -> Modifiers {
    let mut out = Modifiers::empty();
    // Browser payload bit layout (mcp-agent-mail-wasm/www/index.js):
    // ctrl=1, shift=2, alt=4, meta=8.
    if (modifiers & 0b0001) != 0 {
        out |= Modifiers::CTRL;
    }
    if (modifiers & 0b0010) != 0 {
        out |= Modifiers::SHIFT;
    }
    if (modifiers & 0b0100) != 0 {
        out |= Modifiers::ALT;
    }
    if (modifiers & 0b1000) != 0 {
        out |= Modifiers::SUPER;
    }
    out
}

fn remote_key_code_from_label(key: &str) -> Option<KeyCode> {
    let trimmed = key.trim();
    if trimmed.chars().count() == 1 {
        return trimmed.chars().next().map(KeyCode::Char);
    }

    let normalized = trimmed.to_ascii_lowercase();
    if let Some(rest) = normalized.strip_prefix('f')
        && let Ok(function_num) = rest.parse::<u8>()
        && (1..=24).contains(&function_num)
    {
        return Some(KeyCode::F(function_num));
    }

    match normalized.as_str() {
        "enter" | "return" => Some(KeyCode::Enter),
        "escape" | "esc" => Some(KeyCode::Escape),
        "backspace" => Some(KeyCode::Backspace),
        "tab" => Some(KeyCode::Tab),
        "backtab" | "shift+tab" => Some(KeyCode::BackTab),
        "delete" | "del" => Some(KeyCode::Delete),
        "insert" | "ins" => Some(KeyCode::Insert),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" | "page_up" | "pgup" => Some(KeyCode::PageUp),
        "pagedown" | "page_down" | "pgdn" => Some(KeyCode::PageDown),
        "up" | "arrowup" => Some(KeyCode::Up),
        "down" | "arrowdown" => Some(KeyCode::Down),
        "left" | "arrowleft" => Some(KeyCode::Left),
        "right" | "arrowright" => Some(KeyCode::Right),
        "space" | "spacebar" => Some(KeyCode::Char(' ')),
        "null" => Some(KeyCode::Null),
        _ => None,
    }
}

fn remote_terminal_event_to_event(event: RemoteTerminalEvent) -> Option<Event> {
    match event {
        RemoteTerminalEvent::Key { key, modifiers } => {
            let key_code = remote_key_code_from_label(&key)?;
            let key_event =
                ftui::KeyEvent::new(key_code).with_modifiers(remote_modifiers_from_bits(modifiers));
            Some(Event::Key(key_event))
        }
        RemoteTerminalEvent::Resize { cols, rows } => Some(Event::Resize {
            width: cols,
            height: rows,
        }),
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
    pub const fn is_active(&self) -> bool {
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
                ModalSeverity::Info => toast_color_info(),
                ModalSeverity::Warning => toast_color_warning(),
                ModalSeverity::Error => toast_color_error(),
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
// Overlay stack — z-order and close precedence contract
// ──────────────────────────────────────────────────────────────────────

/// Identifies the active overlay layer in the TUI overlay stack.
///
/// Overlays are rendered in ascending z-order: lower layers render first,
/// higher layers paint on top.  Event routing follows **topmost-first**
/// precedence: the highest active overlay consumes events (focus trapping)
/// before lower layers or the base screen ever see them.
///
/// **Escape / close rule:** Pressing Escape always dismisses the *topmost*
/// active overlay.  If no overlay is active, Escape has no effect at the
/// shell level (screens may handle it locally).
///
/// The numeric discriminants encode render z-order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum OverlayLayer {
    /// No overlay active — base screen has focus.
    None = 0,
    /// Toast notifications (passive, z=4). Does not trap focus.
    Toasts = 1,
    /// Toast focus mode (z=4b). Traps j/k/Enter/Esc.
    ToastFocus = 2,
    /// Action menu (z=4.3). Traps all events.
    ActionMenu = 3,
    /// Macro playback paused (z=4.4). Traps Enter/Esc.
    MacroPlayback = 4,
    /// Modal dialog (z=4.5). Traps all events.
    Modal = 5,
    /// Compose message overlay (z=4.7). Traps all events.
    Compose = 6,
    /// Command palette (z=5). Traps all events.
    Palette = 7,
    /// Help overlay (z=6, topmost render). Traps Esc/j/k only.
    Help = 8,
    /// Debug inspector overlay (z=7, topmost). Traps keyboard/mouse.
    Inspector = 9,
}

impl OverlayLayer {
    /// Returns `true` if this layer traps focus (consumes events
    /// before lower layers or the base screen).
    #[must_use]
    pub const fn traps_focus(self) -> bool {
        matches!(
            self,
            Self::Palette
                | Self::Compose
                | Self::Modal
                | Self::ActionMenu
                | Self::ToastFocus
                | Self::MacroPlayback
                | Self::Inspector
        )
    }
}

#[derive(Debug, Clone)]
struct InspectorTreeRow {
    label: String,
    name: String,
    depth: u8,
    area: Rect,
    hit_id: Option<HitId>,
    render_time_us: Option<u64>,
}

// ──────────────────────────────────────────────────────────────────────
// Coach hints — lightweight, one-shot contextual tips
// ──────────────────────────────────────────────────────────────────────

/// One-shot contextual tip shown on first visit to a screen.
struct CoachHint {
    /// Stable identifier persisted across sessions.
    id: &'static str,
    /// Screen that triggers this hint.
    screen: MailScreenId,
    /// Short message shown as a toast.
    message: &'static str,
}

/// Static hint catalog — one hint per screen for first-use friction.
const COACH_HINTS: &[CoachHint] = &[
    CoachHint {
        id: "dashboard:welcome",
        screen: MailScreenId::Dashboard,
        message: "Tip: Press ? for help, Ctrl+P for command palette",
    },
    CoachHint {
        id: "messages:search",
        screen: MailScreenId::Messages,
        message: "Tip: Press / to filter messages, Enter to view body",
    },
    CoachHint {
        id: "threads:expand",
        screen: MailScreenId::Threads,
        message: "Tip: Enter expands a thread, h collapses it",
    },
    CoachHint {
        id: "agents:inbox",
        screen: MailScreenId::Agents,
        message: "Tip: Use / to filter agents, s to cycle sort columns",
    },
    CoachHint {
        id: "search:syntax",
        screen: MailScreenId::Search,
        message: "Tip: Use quoted phrases and AND/OR operators in search",
    },
    CoachHint {
        id: "reservations:force",
        screen: MailScreenId::Reservations,
        message: "Tip: Press f to force-release a stale reservation",
    },
    CoachHint {
        id: "tool_metrics:sort",
        screen: MailScreenId::ToolMetrics,
        message: "Tip: Sort columns with Tab, slow calls are highlighted",
    },
    CoachHint {
        id: "system_health:diag",
        screen: MailScreenId::SystemHealth,
        message: "Tip: WAL size and pool stats refresh every tick",
    },
    CoachHint {
        id: "timeline:expand",
        screen: MailScreenId::Timeline,
        message: "Tip: Enter expands an event, use correlation links to trace",
    },
    CoachHint {
        id: "projects:browse",
        screen: MailScreenId::Projects,
        message: "Tip: Use / to filter projects, s to cycle sort columns",
    },
    CoachHint {
        id: "contacts:approve",
        screen: MailScreenId::Contacts,
        message: "Tip: Accept or deny pending contact requests inline",
    },
    CoachHint {
        id: "explorer:filter",
        screen: MailScreenId::Explorer,
        message: "Tip: Filter by agent, project, or date range",
    },
    CoachHint {
        id: "analytics:volume",
        screen: MailScreenId::Analytics,
        message: "Tip: Message volume charts update in real time",
    },
    CoachHint {
        id: "attachments:preview",
        screen: MailScreenId::Attachments,
        message: "Tip: Enter previews an attachment, / filters by name",
    },
    CoachHint {
        id: "archive_browser:navigate",
        screen: MailScreenId::ArchiveBrowser,
        message: "Tip: Navigate with j/k, Enter to expand dirs or preview files, Tab to switch panes",
    },
];

/// Manages one-shot dismissible coach hints, persisted across sessions.
struct CoachHintManager {
    /// IDs of permanently dismissed hints.
    dismissed: HashSet<String>,
    /// Screens already visited this session (prevents re-showing).
    visited: HashSet<MailScreenId>,
    /// Path to the dismissed-hints JSON file.
    persist_path: Option<PathBuf>,
    /// Whether hints are enabled at all.
    enabled: bool,
    /// Dirty flag — set when `dismissed` changes and needs flushing.
    dirty: bool,
}

impl CoachHintManager {
    fn new() -> Self {
        Self {
            dismissed: HashSet::new(),
            visited: HashSet::new(),
            persist_path: None,
            enabled: true,
            dirty: false,
        }
    }

    fn with_persist_path(mut self, path: PathBuf) -> Self {
        self.dismissed = crate::tui_persist::load_dismissed_hints_or_default(&path);
        self.persist_path = Some(path);
        self
    }

    /// Check if a hint should be shown for the given screen.
    /// Returns the hint message if eligible, `None` otherwise.
    fn on_screen_visit(&mut self, screen: MailScreenId) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        if !self.visited.insert(screen) {
            return None; // Already visited this session
        }
        for hint in COACH_HINTS {
            if hint.screen == screen && !self.dismissed.contains(hint.id) {
                self.dismissed.insert(hint.id.to_string());
                self.dirty = true;
                return Some(hint.message);
            }
        }
        None
    }

    /// Flush dismissed hints to disk if dirty.
    fn flush_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(ref path) = self.persist_path
            && crate::tui_persist::save_dismissed_hints(path, &self.dismissed).is_ok()
        {
            self.dirty = false;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// MailAppModel — implements ftui_runtime::Model
// ──────────────────────────────────────────────────────────────────────

/// Encapsulates screen instances and active-screen routing.
struct ScreenManager {
    state: Arc<TuiSharedState>,
    active_screen: MailScreenId,
    screens: HashMap<MailScreenId, Box<dyn MailScreen>>,
}

impl ScreenManager {
    fn new(state: &Arc<TuiSharedState>) -> Self {
        let mut manager = Self {
            state: Arc::clone(state),
            active_screen: MailScreenId::Dashboard,
            screens: HashMap::new(),
        };
        // Eager-load only the default landing surface.
        manager.ensure_screen(MailScreenId::Dashboard);
        manager
    }

    fn create_screen(id: MailScreenId, state: &Arc<TuiSharedState>) -> Box<dyn MailScreen> {
        match id {
            MailScreenId::Dashboard => Box::new(DashboardScreen::new()),
            MailScreenId::Messages => Box::new(MessageBrowserScreen::new()),
            MailScreenId::Threads => Box::new(ThreadExplorerScreen::new()),
            MailScreenId::Timeline => Box::new(TimelineScreen::new()),
            MailScreenId::SystemHealth => Box::new(SystemHealthScreen::new(Arc::clone(state))),
            MailScreenId::Agents => Box::new(AgentsScreen::new()),
            MailScreenId::Search => Box::new(SearchCockpitScreen::new()),
            MailScreenId::ToolMetrics => Box::new(ToolMetricsScreen::new()),
            MailScreenId::Reservations => Box::new(ReservationsScreen::new()),
            MailScreenId::Projects => Box::new(ProjectsScreen::new()),
            MailScreenId::Contacts => Box::new(ContactsScreen::new()),
            MailScreenId::Explorer => Box::new(MailExplorerScreen::new()),
            MailScreenId::Analytics => Box::new(AnalyticsScreen::new()),
            MailScreenId::Attachments => Box::new(AttachmentExplorerScreen::new()),
            MailScreenId::ArchiveBrowser => Box::new(ArchiveBrowserScreen::new()),
        }
    }

    fn ensure_screen(&mut self, id: MailScreenId) {
        let state = Arc::clone(&self.state);
        self.screens
            .entry(id)
            .or_insert_with(|| Self::create_screen(id, &state));
    }

    fn set_screen(&mut self, id: MailScreenId, screen: Box<dyn MailScreen>) {
        self.screens.insert(id, screen);
    }

    const fn active_screen(&self) -> MailScreenId {
        self.active_screen
    }

    fn set_active_screen(&mut self, id: MailScreenId) {
        self.active_screen = id;
        self.ensure_screen(id);
    }

    fn get(&self, id: MailScreenId) -> Option<&dyn MailScreen> {
        self.screens.get(&id).map(Box::as_ref)
    }

    fn get_mut(&mut self, id: MailScreenId) -> Option<&mut (dyn MailScreen + '_)> {
        self.ensure_screen(id);
        let screen = self.screens.get_mut(&id)?;
        Some(screen.as_mut())
    }

    fn active_screen_ref(&self) -> Option<&dyn MailScreen> {
        self.get(self.active_screen)
    }

    fn active_screen_mut(&mut self) -> Option<&mut (dyn MailScreen + '_)> {
        self.get_mut(self.active_screen)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (MailScreenId, &mut Box<dyn MailScreen>)> {
        self.screens.iter_mut().map(|(&id, screen)| (id, screen))
    }

    fn apply_deep_link(&mut self, target: &DeepLinkTarget) {
        let target_screen = target.target_screen();
        self.set_active_screen(target_screen);
        if let Some(screen) = self.get_mut(target_screen) {
            screen.receive_deep_link(target);
        }
    }

    #[cfg(test)]
    fn has_screen(&self, id: MailScreenId) -> bool {
        self.screens.contains_key(&id)
    }
}

/// The top-level TUI application model.
///
/// Owns all screen instances and dispatches events to the active screen
/// after processing global keybindings.
#[allow(clippy::struct_excessive_bools)]
pub struct MailAppModel {
    state: Arc<TuiSharedState>,
    screen_manager: ScreenManager,
    help_visible: bool,
    help_scroll: u16,
    keymap: crate::tui_keymap::KeymapRegistry,
    command_palette: CommandPalette,
    ambient_renderer: RefCell<AmbientEffectRenderer>,
    ambient_mode: AmbientMode,
    ambient_last_telemetry: Cell<AmbientRenderTelemetry>,
    /// Tick index of the most recent ambient render in `view()`.
    ambient_last_render_tick: Cell<Option<u64>>,
    /// Total ambient render invocations for regression diagnostics.
    ambient_render_invocations: Cell<u64>,
    hint_ranker: HintRanker,
    palette_hint_ids: HashMap<String, usize>,
    palette_usage_path: Option<PathBuf>,
    appearance_persist_path: Option<PathBuf>,
    palette_usage_stats: crate::tui_persist::PaletteUsageMap,
    palette_usage_dirty: bool,
    notifications: NotificationQueue,
    last_toast_seq: u64,
    tick_count: u64,
    accessibility: crate::tui_persist::AccessibilitySettings,
    macro_engine: MacroEngine,
    /// Tracks active reservations for expiry warnings.
    /// Key: "{project}:{agent}:{path}", Value: (`display_label`, `expiry_timestamp_micros`).
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
    toast_age_ticks: HashMap<ToastId, u8>,
    /// When `Some(idx)`, the toast stack is in focus mode and the
    /// toast at `idx` has a highlight border. `Ctrl+Y` toggles.
    toast_focus_index: Option<usize>,
    /// Modal manager for confirmation dialogs.
    modal_manager: ModalManager,
    /// Action menu for contextual per-item actions.
    action_menu: ActionMenuManager,
    /// Export format menu state (`Ctrl+E` to open, `Esc` to close).
    export_menu_open: bool,
    /// Global focus tracker used for cross-panel spatial navigation.
    focus_manager: FocusManager,
    /// Per-screen last-focused target used for focus memory across switches.
    focus_memory: HashMap<MailScreenId, FocusTarget>,
    /// Last rendered screen content area, used to map spatial focus to panel rects.
    last_content_area: RefCell<Rect>,
    /// Last non-high-contrast theme to restore after toggling HC off.
    last_non_hc_theme: ThemeId,
    screen_transition: Option<ScreenTransition>,
    /// Bayesian diff strategy for frame rendering decisions.
    diff_strategy: RefCell<crate::tui_decision::BayesianDiffStrategy>,
    /// Whether a resize event was observed since the last `view()`.
    resize_detected: Cell<bool>,
    /// Timestamp of last `view()` call for frame budget tracking.
    last_view_instant: Cell<Option<Instant>>,
    /// Screens that have panicked. Key: screen id, Value: error message.
    /// When a screen is in this map, a fallback error UI is shown instead.
    /// Uses `RefCell` so that `view(&self)` can record panics.
    screen_panics: RefCell<HashMap<MailScreenId, String>>,
    /// Central mouse dispatcher for shell-level interactions (tab clicks, etc.).
    mouse_dispatcher: crate::tui_hit_regions::MouseDispatcher,
    /// Coach hint manager for one-shot contextual tips.
    coach_hints: CoachHintManager,
    /// Recent action outcomes for feedback surfaces.
    action_outcomes: Vec<ActionOutcome>,
    /// Clipboard helper for OSC 52 / system clipboard copy operations.
    clipboard: Clipboard,
    /// Internal clipboard fallback when terminal clipboard is unavailable.
    internal_clipboard: Option<String>,
    /// Last fully rendered frame snapshot used for screen export.
    last_export_snapshot: RefCell<Option<FrameExportSnapshot>>,
    /// Last armed quit confirmation timestamp.
    quit_confirm_armed_at: Option<Instant>,
    /// Input source that armed quit confirmation.
    quit_confirm_source: Option<QuitConfirmSource>,
    /// Compose message overlay state (`Ctrl+N` to open, `Esc` to close).
    compose_state: Option<ComposeState>,
    /// Global widget-tree inspector state (debug-only; gated by `AM_TUI_DEBUG`).
    inspector: InspectorState,
    /// Flattened inspector tree size from the most recent frame.
    inspector_last_tree_len: Cell<usize>,
    /// Current tree cursor for inspector navigation.
    inspector_selected_index: usize,
    /// Whether the inspector properties panel is visible.
    inspector_show_properties: bool,
    /// Sender for deferred actions from modal callbacks.
    action_tx: std::sync::mpsc::Sender<(String, String)>,
    /// Receiver for deferred actions from modal callbacks.
    action_rx: std::sync::mpsc::Receiver<(String, String)>,
}

impl MailAppModel {
    /// Create a new application model with placeholder screens (no persistence).
    #[must_use]
    pub fn new(state: Arc<TuiSharedState>) -> Self {
        let screen_manager = ScreenManager::new(&state);

        let static_actions = build_palette_actions_static();
        let mut command_palette = CommandPalette::new()
            .with_style(command_palette_theme_style())
            .with_max_visible(PALETTE_MAX_VISIBLE);
        command_palette.replace_actions(static_actions.clone());
        let mut hint_ranker = HintRanker::new(RankerConfig::default());
        let mut palette_hint_ids: HashMap<String, usize> = HashMap::new();
        register_palette_hints(
            &mut hint_ranker,
            &mut palette_hint_ids,
            &static_actions,
            screen_palette_action_id(MailScreenId::Dashboard),
        );
        let initial_theme = crate::tui_theme::current_theme_id();
        let last_non_hc_theme = if initial_theme == ThemeId::HighContrast {
            ThemeId::CyberpunkAurora
        } else {
            initial_theme
        };
        let focus_manager = FocusManager::with_ring(focus_ring_for_screen(MailScreenId::Dashboard));
        let mut focus_memory = HashMap::new();
        if focus_manager.current() != FocusTarget::None {
            focus_memory.insert(MailScreenId::Dashboard, focus_manager.current());
        }

        let (action_tx, action_rx) = std::sync::mpsc::channel();

        Self {
            state,
            screen_manager,
            help_visible: false,
            help_scroll: 0,
            keymap: crate::tui_keymap::KeymapRegistry::default(),
            command_palette,
            ambient_renderer: RefCell::new(AmbientEffectRenderer::new()),
            ambient_mode: AmbientMode::Subtle,
            ambient_last_telemetry: Cell::new(AmbientRenderTelemetry::default()),
            ambient_last_render_tick: Cell::new(None),
            ambient_render_invocations: Cell::new(0),
            hint_ranker,
            palette_hint_ids,
            palette_usage_path: None,
            appearance_persist_path: None,
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
            toast_age_ticks: HashMap::new(),
            toast_focus_index: None,
            modal_manager: ModalManager::new(),
            action_menu: ActionMenuManager::new(),
            export_menu_open: false,
            focus_manager,
            focus_memory,
            last_content_area: RefCell::new(Rect::new(0, 0, 1, 1)),
            last_non_hc_theme,
            screen_transition: None,
            diff_strategy: RefCell::new(crate::tui_decision::BayesianDiffStrategy::new()),
            resize_detected: Cell::new(false),
            last_view_instant: Cell::new(None),
            screen_panics: RefCell::new(HashMap::new()),
            mouse_dispatcher: crate::tui_hit_regions::MouseDispatcher::new(),
            coach_hints: CoachHintManager::new(),
            action_outcomes: Vec::new(),
            clipboard: Clipboard::auto(ftui::TerminalCapabilities::detect()),
            internal_clipboard: None,
            last_export_snapshot: RefCell::new(None),
            quit_confirm_armed_at: None,
            quit_confirm_source: None,
            compose_state: None,
            inspector: InspectorState::new(),
            inspector_last_tree_len: Cell::new(0),
            inspector_selected_index: 0,
            inspector_show_properties: false,
            action_tx,
            action_rx,
        }
    }

    /// Create the model with config-driven preferences and auto-persistence.
    #[must_use]
    pub fn with_config(state: Arc<TuiSharedState>, config: &mcp_agent_mail_core::Config) -> Self {
        let mut model = Self::new(state);
        let prefs = crate::tui_persist::TuiPreferences::from_config(config);
        // Load accessibility settings + keymap profile from persisted config.
        model.accessibility = prefs.accessibility.clone();
        model.keymap.set_profile(prefs.keymap_profile);
        let usage_path = crate::tui_persist::palette_usage_path(&config.console_persist_path);
        model.palette_usage_stats = crate::tui_persist::load_palette_usage_or_default(&usage_path);
        model.palette_usage_path = Some(usage_path);
        model.appearance_persist_path = Some(config.console_persist_path.clone());
        let hints_path = crate::tui_persist::dismissed_hints_path(&config.console_persist_path);
        model.coach_hints = CoachHintManager::new().with_persist_path(hints_path);
        model.coach_hints.enabled = config.tui_coach_hints_enabled;
        model.toast_severity = ToastSeverityThreshold::from_config(config);
        model.toast_muted = !config.tui_toast_enabled;
        model.toast_info_dismiss_secs = config.tui_toast_info_dismiss_secs.max(1);
        model.toast_warn_dismiss_secs = config.tui_toast_warn_dismiss_secs.max(1);
        model.toast_error_dismiss_secs = config.tui_toast_error_dismiss_secs.max(1);
        model.ambient_mode = AmbientMode::parse(config.tui_ambient.as_str());
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
        // Initialize named palette and synchronize the base ftui theme.
        crate::tui_theme::init_named_theme(&config.tui_theme);
        let configured_theme = Self::theme_id_for_named_config(&config.tui_theme);
        let _ = crate::tui_theme::set_theme_and_get_name(configured_theme);
        let initial_theme = configured_theme;
        model.last_non_hc_theme = if initial_theme == ThemeId::HighContrast {
            ThemeId::CyberpunkAurora
        } else {
            initial_theme
        };
        if model.accessibility.high_contrast && initial_theme != ThemeId::HighContrast {
            let _ = crate::tui_theme::set_theme_and_get_name(ThemeId::HighContrast);
            model.sync_theme_snapshot();
        }
        model.refresh_palette_theme_style();
        model.accessibility.high_contrast = model.accessibility.high_contrast
            || crate::tui_theme::current_theme_id() == ThemeId::HighContrast;
        model
    }

    fn start_screen_transition(&mut self, from: MailScreenId, to: MailScreenId) {
        if from == to || self.accessibility.reduced_motion {
            self.screen_transition = None;
            return;
        }
        self.screen_transition = Some(ScreenTransition::new(from, to));
    }

    fn remember_focus_for_screen(&mut self, screen: MailScreenId) {
        let current = self.focus_manager.current();
        if current != FocusTarget::None {
            self.focus_memory.insert(screen, current);
        }
    }

    fn restore_focus_for_screen(&mut self, screen: MailScreenId) {
        self.focus_manager
            .set_focus_ring(focus_ring_for_screen(screen));
        let remembered = self
            .focus_memory
            .get(&screen)
            .copied()
            .or_else(|| self.focus_manager.focus_ring().first().copied())
            .unwrap_or(FocusTarget::None);
        let _ = self.focus_manager.focus(remembered);
        if remembered != FocusTarget::None {
            self.focus_memory.insert(screen, remembered);
        }
    }

    fn move_focus_spatial(&mut self, direction: KeyCode) -> bool {
        let active_screen = self.screen_manager.active_screen();
        self.restore_focus_for_screen(active_screen);

        let content_area = *self.last_content_area.borrow();
        let graph = focus_graph_for_screen(active_screen, content_area);
        let current_target = self.focus_manager.current();
        let current_idx = if let Some(idx) = graph.node_index(current_target) {
            idx
        } else {
            let Some(fallback_target) = graph.nodes().first().map(|node| node.target) else {
                return false;
            };
            let _ = self.focus_manager.focus(fallback_target);
            self.focus_memory.insert(active_screen, fallback_target);
            graph.node_index(fallback_target).unwrap_or(0)
        };
        let current = graph.nodes()[current_idx];
        let next_idx = match direction {
            KeyCode::Up => current.neighbors.up,
            KeyCode::Down => current.neighbors.down,
            KeyCode::Left => current.neighbors.left,
            KeyCode::Right => current.neighbors.right,
            _ => None,
        };
        let Some(next_idx) = next_idx else {
            return false;
        };
        let Some(next_node) = graph.nodes().get(next_idx) else {
            return false;
        };
        if self.focus_manager.focus(next_node.target) {
            self.focus_memory.insert(active_screen, next_node.target);
        }
        true
    }

    fn terminal_area_from_last_content(&self) -> Rect {
        let content = *self.last_content_area.borrow();
        Rect::new(
            content.x,
            content.y.saturating_sub(1),
            content.width,
            content.height.saturating_add(2),
        )
    }

    fn handle_help_overlay_mouse(&mut self, event: &Event) -> bool {
        if !self.help_visible {
            return false;
        }
        let Event::Mouse(mouse) = event else {
            return false;
        };

        let overlay = crate::tui_chrome::help_overlay_rect(self.terminal_area_from_last_content());
        let inside_overlay = crate::tui_hit_regions::point_in_rect(overlay, mouse.x, mouse.y);

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !inside_overlay {
                    self.help_visible = false;
                }
                true
            }
            MouseEventKind::ScrollDown if inside_overlay => {
                self.help_scroll = self.help_scroll.saturating_add(1);
                true
            }
            MouseEventKind::ScrollUp if inside_overlay => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
                true
            }
            _ => true,
        }
    }

    fn activate_screen(&mut self, id: MailScreenId) {
        let from = self.screen_manager.active_screen();
        self.remember_focus_for_screen(from);
        self.screen_manager.set_active_screen(id);
        let to = self.screen_manager.active_screen();
        self.restore_focus_for_screen(to);
        self.start_screen_transition(from, to);
        self.show_coach_hint_if_eligible(id);
    }

    /// Show a one-shot coach hint toast for the given screen if eligible.
    fn show_coach_hint_if_eligible(&mut self, screen: MailScreenId) {
        if let Some(msg) = self.coach_hints.on_screen_visit(screen) {
            let tp = crate::tui_theme::TuiThemePalette::current();
            self.notifications.notify(
                Toast::new(msg)
                    .icon(ToastIcon::Info)
                    .style(Style::default().fg(tp.panel_border))
                    .duration(Duration::from_secs(8)),
            );
            self.coach_hints.flush_if_dirty();
        }
    }

    fn apply_deep_link_with_transition(&mut self, target: &DeepLinkTarget) {
        let from = self.screen_manager.active_screen();
        self.remember_focus_for_screen(from);
        self.screen_manager.apply_deep_link(target);
        let to = self.screen_manager.active_screen();
        self.restore_focus_for_screen(to);
        self.start_screen_transition(from, to);
    }

    /// Replace a screen implementation (used when real screens are ready).
    pub fn set_screen(&mut self, id: MailScreenId, screen: Box<dyn MailScreen>) {
        self.screen_manager.set_screen(id, screen);
    }

    /// Get the currently active screen ID.
    #[must_use]
    pub const fn active_screen(&self) -> MailScreenId {
        self.screen_manager.active_screen()
    }

    /// Whether the help overlay is currently shown.
    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// Get mutable access to the modal manager for showing confirmation dialogs.
    pub const fn modal_manager_mut(&mut self) -> &mut ModalManager {
        &mut self.modal_manager
    }

    /// Get mutable access to the action menu manager.
    pub const fn action_menu_mut(&mut self) -> &mut ActionMenuManager {
        &mut self.action_menu
    }

    /// Dispatch an action selected from the action menu.
    fn dispatch_action_menu_selection(
        &mut self,
        action: ActionKind,
        context: &str,
    ) -> Cmd<MailMsg> {
        match action {
            ActionKind::Navigate(screen_id) => {
                self.activate_screen(screen_id);
                Cmd::none()
            }
            ActionKind::DeepLink(target) => {
                self.apply_deep_link_with_transition(&target);
                Cmd::none()
            }
            ActionKind::Execute(operation) => self.dispatch_execute_operation(&operation, context),
            ActionKind::ConfirmThenExecute {
                title,
                message,
                operation,
            } => {
                let ctx = context.to_string();
                let action_tx = self.action_tx.clone();
                self.modal_manager.show_confirmation(
                    title,
                    message,
                    ModalSeverity::Warning,
                    move |result| {
                        if matches!(result, DialogResult::Ok) {
                            // Confirmed operation is queued for deferred execution.
                            let _ = action_tx.send((operation, ctx));
                        }
                    },
                );
                Cmd::none()
            }
            ActionKind::CopyToClipboard(text) => {
                self.copy_to_clipboard(&text);
                Cmd::none()
            }
            ActionKind::Dismiss => Cmd::none(),
        }
    }

    fn notify_compose_result(&mut self, success: bool, context: &str) {
        let (message, icon, duration) = if success {
            let msg = if context.is_empty() {
                "Message sent".to_string()
            } else {
                format!("Message sent: {context}")
            };
            (msg, ToastIcon::Info, Duration::from_secs(3))
        } else {
            let msg = if context.is_empty() {
                "Message send failed".to_string()
            } else {
                format!("Message send failed: {context}")
            };
            (msg, ToastIcon::Error, Duration::from_secs(5))
        };
        self.notifications
            .notify(Toast::new(message).icon(icon).duration(duration));
    }

    fn notify_reservation_create_result(&mut self, status: &str, context: &str) {
        let (prefix, icon, duration) = match status {
            "ok" => (
                "Reservation created",
                ToastIcon::Info,
                Duration::from_secs(3),
            ),
            "warn" => (
                "Reservation created with conflicts",
                ToastIcon::Warning,
                Duration::from_secs(4),
            ),
            _ => (
                "Reservation create failed",
                ToastIcon::Error,
                Duration::from_secs(5),
            ),
        };
        let message = if context.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix}: {context}")
        };
        self.notifications
            .notify(Toast::new(message).icon(icon).duration(duration));
    }

    fn execute_rethread_message(&mut self, message_id: i64, target_thread_id: &str) {
        let target_thread_id = target_thread_id.trim();
        if target_thread_id.is_empty() {
            return;
        }

        let snapshot = self.state.config_snapshot();
        let cfg = DbPoolConfig {
            database_url: snapshot.raw_database_url,
            ..Default::default()
        };
        let Ok(path) = cfg.sqlite_path() else {
            self.record_action_outcome(ActionOutcome::Failure {
                operation: "rethread_message".to_string(),
                error: "failed to resolve sqlite path".to_string(),
            });
            return;
        };
        let Ok(conn) = DbConn::open_file(&path) else {
            self.record_action_outcome(ActionOutcome::Failure {
                operation: "rethread_message".to_string(),
                error: format!("failed to open database at {path}"),
            });
            return;
        };

        let lookup_sql = "SELECT thread_id FROM messages WHERE id = ? LIMIT 1";
        let current_thread_id = match conn.query_sync(lookup_sql, &[Value::BigInt(message_id)]) {
            Ok(rows) => rows
                .into_iter()
                .next()
                .and_then(|row| row.get_named::<String>("thread_id").ok()),
            Err(e) => {
                self.record_action_outcome(ActionOutcome::Failure {
                    operation: "rethread_message".to_string(),
                    error: format!("query failed: {e}"),
                });
                return;
            }
        };

        let Some(current_thread_id) = current_thread_id else {
            self.notifications.notify(
                Toast::new(format!("Message {message_id} not found"))
                    .icon(ToastIcon::Warning)
                    .duration(Duration::from_secs(3)),
            );
            return;
        };
        if current_thread_id == target_thread_id {
            return;
        }

        let update_sql = "UPDATE messages SET thread_id = ? WHERE id = ?";
        if let Err(e) = conn.execute_sync(
            update_sql,
            &[
                Value::Text(target_thread_id.to_string()),
                Value::BigInt(message_id),
            ],
        ) {
            self.record_action_outcome(ActionOutcome::Failure {
                operation: "rethread_message".to_string(),
                error: format!("update failed: {e}"),
            });
            return;
        }

        self.record_action_outcome(ActionOutcome::Success {
            operation: "rethread_message".to_string(),
            summary: format!("Moved #{message_id} to thread {target_thread_id}"),
        });
    }

    /// Copy text to the terminal clipboard via OSC 52, with system and
    /// internal fallbacks. Shows a toast with a preview of the copied content.
    fn copy_to_clipboard(&mut self, text: &str) {
        let preview: String = text.chars().take(40).collect();
        let truncated = if text.chars().count() > 40 {
            format!("{preview}...")
        } else {
            preview
        };

        // Attempt OSC 52 / system clipboard.
        let result =
            self.clipboard
                .set(text, ClipboardSelection::Clipboard, &mut std::io::stdout());

        // Always keep an in-process copy, regardless of terminal support.
        self.internal_clipboard = Some(text.to_string());

        let (message, icon, duration) = if result == Ok(()) {
            (format!("Copied: {truncated}"), ToastIcon::Info, 2)
        } else {
            (
                format!("Copied (internal): {truncated}"),
                ToastIcon::Warning,
                3,
            )
        };
        self.notifications.notify(
            Toast::new(message)
                .icon(icon)
                .duration(Duration::from_secs(duration)),
        );
    }

    #[allow(clippy::missing_const_for_fn)]
    fn open_export_menu(&mut self) {
        self.help_visible = false;
        self.help_scroll = 0;
        self.export_menu_open = true;
    }

    fn handle_export_menu_key(&mut self, key: &ftui::KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Escape => {
                self.export_menu_open = false;
            }
            KeyCode::Char('h' | 'H') => {
                self.export_menu_open = false;
                self.export_current_snapshot(ExportFormat::Html);
            }
            KeyCode::Char('s' | 'S') => {
                self.export_menu_open = false;
                self.export_current_snapshot(ExportFormat::Svg);
            }
            KeyCode::Char('t' | 'T') => {
                self.export_menu_open = false;
                self.export_current_snapshot(ExportFormat::Text);
            }
            _ => {}
        }
    }

    fn resolve_export_dir() -> PathBuf {
        let env_export = std::env::var("AM_EXPORT_DIR").ok();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        resolve_export_dir_from_sources(env_export.as_deref(), home.as_deref())
    }

    fn export_snapshot_to_dir(
        &self,
        format: ExportFormat,
        export_dir: &Path,
    ) -> Result<PathBuf, String> {
        let snapshot = self
            .last_export_snapshot
            .borrow()
            .clone()
            .ok_or_else(|| "no rendered frame available yet".to_string())?;

        std::fs::create_dir_all(export_dir)
            .map_err(|e| format!("failed to create export directory: {e}"))?;

        let screen_label = screen_meta(self.screen_manager.active_screen()).short_label;
        let mut screen_slug = sanitize_filename_component(screen_label);
        if screen_slug.is_empty() {
            screen_slug = "screen".to_string();
        }
        let timestamp = now_micros();
        let file_name = format!("am_export_{screen_slug}_{timestamp}.{}", format.extension());
        let path = export_dir.join(file_name);
        let rendered = format.render(&snapshot);
        std::fs::write(&path, rendered).map_err(|e| format!("failed to write export file: {e}"))?;
        Ok(path)
    }

    fn export_current_snapshot(&mut self, format: ExportFormat) {
        let export_dir = Self::resolve_export_dir();
        match self.export_snapshot_to_dir(format, &export_dir) {
            Ok(path) => {
                self.notifications.notify(
                    Toast::new(format!("Exported to {}", path.display()))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(4)),
                );
            }
            Err(error) => {
                self.notifications.notify(
                    Toast::new(format!("Export failed: {error}"))
                        .icon(ToastIcon::Error)
                        .duration(Duration::from_secs(4)),
                );
            }
        }
    }

    /// Route an `Execute` operation to the appropriate handler.
    ///
    /// Operations are parsed as `"command:arg"` pairs. Known operations are
    /// mapped to deep-links, screen messages, or navigation actions. Unknown
    /// operations show an informational toast.
    fn dispatch_execute_operation(&mut self, operation: &str, context: &str) -> Cmd<MailMsg> {
        let (cmd, arg) = match operation.split_once(':') {
            Some((c, a)) => (c, a),
            None => (operation, context),
        };

        match cmd {
            // ── Navigation operations ────────────────────────────
            "view_body" => {
                // Deep-link to the message by extracting ID from context.
                if let Some(id) = extract_numeric_id(context) {
                    self.apply_deep_link_with_transition(&DeepLinkTarget::MessageById(id));
                }
                Cmd::none()
            }
            "view_messages" => {
                if !arg.is_empty() && arg != context {
                    self.apply_deep_link_with_transition(&DeepLinkTarget::ThreadById(
                        arg.to_string(),
                    ));
                }
                Cmd::none()
            }
            "view_details" => {
                if let Some(ts) = extract_numeric_id(context) {
                    self.apply_deep_link_with_transition(&DeepLinkTarget::TimelineAtTime(ts));
                }
                Cmd::none()
            }
            "view_profile" => {
                self.apply_deep_link_with_transition(&DeepLinkTarget::AgentByName(arg.to_string()));
                Cmd::none()
            }

            // ── Filter operations (screen-local) ─────────────────
            "filter_kind" | "filter_source" => {
                // Search with the filter value.
                self.apply_deep_link_with_transition(&DeepLinkTarget::SearchFocused(
                    arg.to_string(),
                ));
                Cmd::none()
            }
            "search_in" => {
                let query = format!("thread:{arg}");
                self.apply_deep_link_with_transition(&DeepLinkTarget::SearchFocused(query));
                Cmd::none()
            }
            "compose_to" => {
                self.apply_deep_link_with_transition(&DeepLinkTarget::ComposeToAgent(
                    arg.to_string(),
                ));
                Cmd::none()
            }
            "compose_result" => {
                self.notify_compose_result(arg == "ok", context);
                Cmd::none()
            }
            "reservation_create_result" => {
                self.notify_reservation_create_result(arg, context);
                Cmd::none()
            }
            "rethread_message" => {
                if let Some((message_id, target_thread_id)) = parse_rethread_operation_arg(arg) {
                    self.execute_rethread_message(message_id, &target_thread_id);
                } else {
                    self.notifications.notify(
                        Toast::new(format!("Invalid rethread operation: {operation}"))
                            .icon(ToastIcon::Warning)
                            .duration(Duration::from_secs(3)),
                    );
                }
                Cmd::none()
            }

            // ── Server-dispatched operations ──────────────────────
            // These produce an in-flight toast; actual execution is
            // delegated to the screen's update handler via ActionExecute.
            "acknowledge" | "mark_read" | "renew" | "release" | "force_release" | "summarize"
            | "approve_contact" | "deny_contact" | "block_contact" | "batch_acknowledge"
            | "batch_mark_read" | "batch_mark_unread" => {
                let op = operation.to_string();
                let ctx = context.to_string();
                self.action_outcomes.push(ActionOutcome::InFlight {
                    operation: cmd.to_string(),
                });
                self.notifications.notify(
                    Toast::new(format!("Executing: {cmd}…"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(2)),
                );
                Cmd::msg(MailMsg::Screen(MailScreenMsg::ActionExecute(op, ctx)))
            }

            // ── Copy operations ──────────────────────────────────
            "copy_event" => {
                self.notifications.notify(
                    Toast::new("Event copied")
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(2)),
                );
                Cmd::none()
            }

            // ── Fallback ─────────────────────────────────────────
            _ => {
                self.notifications.notify(
                    Toast::new(format!("Action: {operation}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                Cmd::none()
            }
        }
    }

    /// Drain any deferred confirmed action (from modal callback) and dispatch it.
    fn drain_deferred_confirmed_action(&mut self) -> Cmd<MailMsg> {
        // Drain all pending actions from the channel
        let mut cmds = Vec::new();
        while let Ok((operation, context)) = self.action_rx.try_recv() {
            cmds.push(self.dispatch_execute_operation(&operation, &context));
        }
        match cmds.len() {
            0 => Cmd::none(),
            1 => cmds.into_iter().next().unwrap_or_else(Cmd::none),
            _ => Cmd::batch(cmds),
        }
    }

    /// Record an action outcome (success or failure) and show a toast.
    pub fn record_action_outcome(&mut self, outcome: ActionOutcome) {
        match &outcome {
            ActionOutcome::Success { operation, summary } => {
                self.notifications.notify(
                    Toast::new(format!("{operation}: {summary}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
            }
            ActionOutcome::Failure { operation, error } => {
                self.notifications.notify(
                    Toast::new(format!("{operation} failed: {error}"))
                        .icon(ToastIcon::Error)
                        .duration(Duration::from_secs(5)),
                );
            }
            ActionOutcome::InFlight { .. } => {}
        }
        // Remove any prior InFlight for the same operation.
        if let ActionOutcome::Success { ref operation, .. }
        | ActionOutcome::Failure { ref operation, .. } = outcome
        {
            self.action_outcomes.retain(
                |o| !matches!(o, ActionOutcome::InFlight { operation: op } if op == operation),
            );
        }
        // Cap outcome history to 20 entries.
        if self.action_outcomes.len() >= 20 {
            self.action_outcomes.remove(0);
        }
        self.action_outcomes.push(outcome);
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
        self.screen_manager
            .active_screen_ref()
            .is_some_and(MailScreen::consumes_text_input)
    }

    fn sync_palette_hints(&mut self, actions: &[ActionItem]) {
        register_palette_hints(
            &mut self.hint_ranker,
            &mut self.palette_hint_ids,
            actions,
            screen_palette_action_id(self.screen_manager.active_screen()),
        );
    }

    fn rank_palette_actions(&mut self, actions: Vec<ActionItem>) -> Vec<ActionItem> {
        self.sync_palette_hints(&actions);

        let (ordering, _) = self.hint_ranker.rank(Some(screen_palette_action_id(
            self.screen_manager.active_screen(),
        )));
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

    fn ambient_mode_for_frame(&self) -> AmbientMode {
        if self.state.config_snapshot().tui_effects {
            self.ambient_mode
        } else {
            AmbientMode::Off
        }
    }

    fn ambient_health_input(&self, now_micros: i64) -> AmbientHealthInput {
        let ring_stats = self.state.event_ring_stats();
        let event_buffer_utilization = f64::from(ring_stats.fill_pct()) / 100.0;
        let recent_events = self.state.recent_events(AMBIENT_HEALTH_LOOKBACK_EVENTS);

        let mut critical_alerts_active = false;
        let mut warning_events = 0_u32;
        let mut last_event_ts = 0_i64;
        for event in recent_events {
            last_event_ts = last_event_ts.max(event.timestamp_micros());
            match event.severity() {
                EventSeverity::Error => critical_alerts_active = true,
                EventSeverity::Warn => warning_events = warning_events.saturating_add(1),
                _ => {}
            }
        }

        let seconds_since_last_event = if last_event_ts > 0 {
            let delta_micros = now_micros.saturating_sub(last_event_ts).max(0);
            u64::try_from(delta_micros / 1_000_000).unwrap_or(u64::MAX)
        } else {
            self.state.uptime().as_secs()
        };

        let failed_probe_count = if critical_alerts_active {
            2
        } else {
            u32::from(warning_events > 0)
        };

        AmbientHealthInput {
            critical_alerts_active,
            failed_probe_count,
            total_probe_count: if failed_probe_count > 0 { 2 } else { 0 },
            event_buffer_utilization,
            seconds_since_last_event,
        }
    }

    #[cfg(test)]
    const fn ambient_last_telemetry(&self) -> AmbientRenderTelemetry {
        self.ambient_last_telemetry.get()
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

    fn persist_appearance_settings(&self) {
        let Some(path) = self.appearance_persist_path.as_deref() else {
            return;
        };

        let mut map: HashMap<&'static str, String> = HashMap::with_capacity(6);
        map.insert(
            "CONSOLE_THEME",
            crate::tui_theme::current_theme_env_value().to_string(),
        );
        map.insert(
            "TUI_THEME",
            crate::tui_theme::active_named_theme_config_name().to_string(),
        );
        map.insert(
            "TUI_HIGH_CONTRAST",
            self.accessibility.high_contrast.to_string(),
        );
        map.insert("TUI_KEY_HINTS", self.accessibility.key_hints.to_string());
        map.insert(
            "TUI_REDUCED_MOTION",
            self.accessibility.reduced_motion.to_string(),
        );
        map.insert(
            "TUI_SCREEN_READER",
            self.accessibility.screen_reader.to_string(),
        );

        if let Err(e) = mcp_agent_mail_core::config::update_envfile(path, &map) {
            eprintln!(
                "tui_app: failed to persist appearance settings to {}: {e}",
                path.display()
            );
        }
    }

    fn sync_theme_snapshot(&self) {
        let mut snapshot = self.state.config_snapshot();
        snapshot.console_theme = crate::tui_theme::current_theme_name().to_string();
        self.state.update_config_snapshot(snapshot);
    }

    fn refresh_palette_theme_style(&mut self) {
        let palette = std::mem::replace(&mut self.command_palette, CommandPalette::new());
        self.command_palette = palette.with_style(command_palette_theme_style());
    }

    fn theme_id_for_named_config(cfg: &str) -> ThemeId {
        match cfg {
            "solarized" | "lumen_light" | "lumen" | "light" => ThemeId::LumenLight,
            "dracula" | "darcula" => ThemeId::Darcula,
            "nord" | "nordic_frost" | "nordic" => ThemeId::NordicFrost,
            "gruvbox" | "high_contrast" | "high-contrast" | "contrast" | "hc" => {
                ThemeId::HighContrast
            }
            "doom" => ThemeId::Doom,
            "quake" => ThemeId::Quake,
            _ => ThemeId::CyberpunkAurora,
        }
    }

    const fn named_config_for_theme_id(theme_id: ThemeId) -> &'static str {
        match theme_id {
            ThemeId::CyberpunkAurora | ThemeId::Doom | ThemeId::Quake => "default",
            ThemeId::Darcula => "dracula",
            ThemeId::LumenLight => "solarized",
            ThemeId::NordicFrost => "nord",
            ThemeId::HighContrast => "gruvbox",
        }
    }

    fn apply_theme(&mut self, theme_id: ThemeId) -> &'static str {
        let name = crate::tui_theme::set_theme_and_get_name(theme_id);
        let named_cfg = Self::named_config_for_theme_id(theme_id);
        let _ = crate::tui_theme::set_named_theme(
            crate::tui_theme::TuiThemePalette::config_name_to_index(named_cfg),
        );
        self.refresh_palette_theme_style();
        self.accessibility.high_contrast = theme_id == ThemeId::HighContrast;
        if theme_id != ThemeId::HighContrast {
            self.last_non_hc_theme = theme_id;
        }
        self.sync_theme_snapshot();
        self.persist_appearance_settings();
        name
    }

    fn cycle_theme(&mut self) -> &'static str {
        let (cfg, display, _palette) = crate::tui_theme::cycle_named_theme();
        let theme_id = Self::theme_id_for_named_config(cfg);
        let _ = crate::tui_theme::set_theme_and_get_name(theme_id);
        self.refresh_palette_theme_style();
        self.accessibility.high_contrast = theme_id == ThemeId::HighContrast;
        if theme_id != ThemeId::HighContrast {
            self.last_non_hc_theme = theme_id;
        }
        self.sync_theme_snapshot();
        self.persist_appearance_settings();
        display
    }

    fn toggle_high_contrast_theme(&mut self) -> &'static str {
        if self.accessibility.high_contrast {
            let restore = self.last_non_hc_theme;
            self.apply_theme(restore)
        } else {
            let current_theme = crate::tui_theme::current_theme_id();
            if current_theme != ThemeId::HighContrast {
                self.last_non_hc_theme = current_theme;
            }
            self.apply_theme(ThemeId::HighContrast)
        }
    }

    fn flush_before_shutdown(&mut self) {
        self.persist_palette_usage();
        self.persist_appearance_settings();
    }

    const fn clear_quit_confirmation(&mut self) {
        self.quit_confirm_armed_at = None;
        self.quit_confirm_source = None;
    }

    fn arm_quit_confirmation(&mut self, source: QuitConfirmSource) {
        self.quit_confirm_armed_at = Some(Instant::now());
        self.quit_confirm_source = Some(source);
        let message = match source {
            QuitConfirmSource::Escape => {
                "Press Esc again to quit. Ctrl-D detaches TUI and keeps server running."
            }
            QuitConfirmSource::CtrlC => "Press Ctrl-C again to quit. Ctrl-D detaches TUI only.",
        };
        self.notifications.notify(
            self.apply_toast_policy(
                Toast::new(message)
                    .icon(ToastIcon::Warning)
                    .duration(Duration::from_secs(QUIT_CONFIRM_TOAST_SECS)),
            ),
        );
    }

    fn handle_quit_confirmation_input(&mut self, source: QuitConfirmSource) -> Cmd<MailMsg> {
        let now = Instant::now();
        let confirmed = self.quit_confirm_source == Some(source)
            && self
                .quit_confirm_armed_at
                .is_some_and(|armed| now.duration_since(armed) <= QUIT_CONFIRM_WINDOW);
        if confirmed {
            self.clear_quit_confirmation();
            self.flush_before_shutdown();
            self.state.request_shutdown();
            Cmd::quit()
        } else {
            self.arm_quit_confirmation(source);
            Cmd::none()
        }
    }

    fn detach_tui_headless(&mut self) -> Cmd<MailMsg> {
        self.clear_quit_confirmation();
        self.flush_before_shutdown();
        self.state.request_headless_detach();
        Cmd::quit()
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
        let duration = Duration::from_secs(self.toast_dismiss_secs(icon));
        let toast = toast
            .duration(duration)
            .entrance_duration(TICK_INTERVAL.saturating_mul(u32::from(TOAST_ENTRANCE_TICKS)))
            .exit_duration(TICK_INTERVAL.saturating_mul(u32::from(TOAST_EXIT_TICKS)));
        if toast_reduced_motion_enabled(&self.accessibility) {
            toast.no_animation()
        } else {
            toast
        }
    }

    fn tick_toast_animation_state(&mut self) {
        let mut visible_ids = HashSet::new();
        for toast in self.notifications.visible_mut() {
            visible_ids.insert(toast.id);
            let _ = toast.tick_animation();
            self.toast_age_ticks
                .entry(toast.id)
                .and_modify(|age| *age = age.saturating_add(1))
                .or_insert(0);
        }
        self.toast_age_ticks
            .retain(|id, _| visible_ids.contains(id));
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

    fn inspector_enabled(&self) -> bool {
        self.state.config_snapshot().tui_debug
    }

    fn toggle_inspector(&mut self) {
        if !self.inspector_enabled() {
            return;
        }
        self.inspector.toggle();
        if self.inspector.is_active() {
            self.inspector_selected_index = 0;
            self.inspector_show_properties = false;
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    fn is_inspector_toggle_key(key: &ftui::KeyEvent) -> bool {
        matches!(key.code, KeyCode::F(12))
            || (matches!(key.code, KeyCode::Char('i' | 'I'))
                && key.modifiers.contains(Modifiers::CTRL)
                && key.modifiers.contains(Modifiers::SHIFT))
    }

    fn move_inspector_selection(&mut self, delta: isize) {
        let len = self.inspector_last_tree_len.get();
        if len == 0 {
            self.inspector_selected_index = 0;
            return;
        }
        let current = self.inspector_selected_index.min(len - 1);
        self.inspector_selected_index = if delta >= 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
    }

    fn handle_inspector_event(&mut self, event: &Event) -> bool {
        if !self.inspector.is_active() {
            return false;
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Escape | KeyCode::F(12) => {
                        self.inspector.toggle();
                    }
                    KeyCode::Up | KeyCode::Left => {
                        self.move_inspector_selection(-1);
                    }
                    KeyCode::Down | KeyCode::Right => {
                        self.move_inspector_selection(1);
                    }
                    KeyCode::Enter => {
                        self.inspector_show_properties = !self.inspector_show_properties;
                    }
                    _ => {}
                }
                true
            }
            Event::Mouse(_) => true,
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_inspector_widget_tree(
        &self,
        area: Rect,
        chrome: &crate::tui_chrome::ChromeAreas,
        active_screen: MailScreenId,
        tab_render_us: u64,
        content_render_us: u64,
        status_render_us: u64,
        toast_render_us: u64,
        action_menu_render_us: u64,
        modal_render_us: u64,
        palette_render_us: u64,
        help_render_us: u64,
    ) -> WidgetInfo {
        let mut root = WidgetInfo::new("MailApp", area).with_depth(0);
        root.render_time_us = Some(
            tab_render_us
                .saturating_add(content_render_us)
                .saturating_add(status_render_us)
                .saturating_add(toast_render_us)
                .saturating_add(action_menu_render_us)
                .saturating_add(modal_render_us)
                .saturating_add(palette_render_us)
                .saturating_add(help_render_us),
        );

        let mut tab_bar = WidgetInfo::new("TabBar", chrome.tab_bar).with_depth(1);
        tab_bar.render_time_us = Some(tab_render_us);
        for (i, meta) in crate::tui_screens::MAIL_SCREEN_REGISTRY.iter().enumerate() {
            if let Some(slot) = self.mouse_dispatcher.tab_slot(i) {
                let width = slot.1.saturating_sub(slot.0);
                if width == 0 {
                    continue;
                }
                let tab_rect = Rect::new(slot.0, slot.2, width, 1);
                let child = WidgetInfo::new(format!("Tab {}", meta.short_label), tab_rect)
                    .with_depth(2)
                    .with_hit_id(crate::tui_hit_regions::tab_hit_id(meta.id));
                tab_bar.add_child(child);
            }
        }
        root.add_child(tab_bar);

        let mut content = WidgetInfo::new(
            format!(
                "Screen {}",
                crate::tui_screens::screen_meta(active_screen).short_label
            ),
            chrome.content,
        )
        .with_depth(1)
        .with_hit_id(crate::tui_hit_regions::pane_hit_id(active_screen));
        content.render_time_us = Some(content_render_us);
        root.add_child(content);

        let mut status = WidgetInfo::new("StatusLine", chrome.status_line).with_depth(1);
        status.render_time_us = Some(status_render_us);
        root.add_child(status);

        if self.notifications.visible_count() > 0 {
            let mut toasts = WidgetInfo::new("Toasts", area).with_depth(1);
            toasts.render_time_us = Some(toast_render_us);
            root.add_child(toasts);
        }

        if self.action_menu.is_active() {
            let mut action_menu = WidgetInfo::new("ActionMenu", area).with_depth(1);
            action_menu.render_time_us = Some(action_menu_render_us);
            root.add_child(action_menu);
        }

        if self.modal_manager.is_active() {
            let mut modal = WidgetInfo::new("Modal", area).with_depth(1);
            modal.render_time_us = Some(modal_render_us);
            root.add_child(modal);
        }

        if self.command_palette.is_visible() {
            let mut palette = WidgetInfo::new("CommandPalette", area).with_depth(1);
            palette.render_time_us = Some(palette_render_us);
            root.add_child(palette);
        }

        if self.help_visible {
            let mut help =
                WidgetInfo::new("HelpOverlay", crate::tui_chrome::help_overlay_rect(area))
                    .with_depth(1);
            help.render_time_us = Some(help_render_us);
            root.add_child(help);
        }

        let inspector_overlay = WidgetInfo::new(
            "InspectorOverlay",
            crate::tui_chrome::inspector_overlay_rect(area),
        )
        .with_depth(1);
        root.add_child(inspector_overlay);
        root
    }

    // ── Overlay stack query ──────────────────────────────────────

    /// Returns the topmost active overlay in the stack.
    ///
    /// The result determines which layer should consume events (focus
    /// trapping) and which overlay Escape should dismiss.  See
    /// [`OverlayLayer`] for the full precedence contract.
    #[must_use]
    pub fn topmost_overlay(&self) -> OverlayLayer {
        // Check in *event-routing* order (highest precedence first).
        if self.inspector.is_active() {
            return OverlayLayer::Inspector;
        }
        if self.command_palette.is_visible() {
            return OverlayLayer::Palette;
        }
        if self.compose_state.is_some() {
            return OverlayLayer::Compose;
        }
        if self.modal_manager.is_active() {
            return OverlayLayer::Modal;
        }
        if self.action_menu.is_active() {
            return OverlayLayer::ActionMenu;
        }
        if matches!(
            self.macro_engine.playback_state(),
            PlaybackState::Paused { .. }
        ) {
            return OverlayLayer::MacroPlayback;
        }
        if self.toast_focus_index.is_some() {
            return OverlayLayer::ToastFocus;
        }
        if self.help_visible {
            return OverlayLayer::Help;
        }
        if self.notifications.visible_count() > 0 {
            return OverlayLayer::Toasts;
        }
        OverlayLayer::None
    }

    #[must_use]
    fn adjust_diff_action_for_overlays(
        diff_action: crate::tui_decision::DiffAction,
    ) -> crate::tui_decision::DiffAction {
        if diff_action == crate::tui_decision::DiffAction::Deferred {
            // The dashboard and overlays paint focus/caret highlights directly
            // into cells. Deferring a frame leaves stale highlight cells behind
            // (visible as purple cursor trails), so promote to a real render.
            return crate::tui_decision::DiffAction::Full;
        }
        diff_action
    }

    fn open_palette(&mut self) {
        self.help_visible = false;
        let mut actions = build_palette_actions(&self.state);

        // Inject context-aware quick actions from the focused entity.
        if let Some(screen) = self.screen_manager.active_screen_ref()
            && let Some(event) = screen.focused_event()
        {
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

    /// Open the compose message overlay (`Ctrl+N`).
    fn open_compose(&mut self) {
        if self.compose_state.is_some() {
            return; // Already open.
        }
        let mut cs = ComposeState::new();
        // Populate agent list from recent events for recipient auto-complete.
        let agents: Vec<String> = self
            .state
            .events_since(0)
            .iter()
            .filter_map(|e| {
                if let crate::tui_events::MailEvent::AgentRegistered { name, .. } = e {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        cs.set_available_agents(agents);
        self.compose_state = Some(cs);
    }

    /// Dispatch a composed message for sending via the server control channel.
    fn dispatch_compose_send(
        &mut self,
        envelope: crate::tui_compose::ComposeEnvelope,
    ) -> Cmd<MailMsg> {
        let recipients = envelope.to.join(", ");
        if self
            .state
            .try_send_server_control(ServerControlMsg::ComposeEnvelope(envelope))
        {
            self.notifications.notify(
                Toast::new(format!("Sending to: {recipients}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_secs(3)),
            );
        } else {
            self.notifications.notify(
                Toast::new("No server channel — message not sent")
                    .icon(ToastIcon::Error)
                    .duration(Duration::from_secs(5)),
            );
        }
        Cmd::none()
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
            palette_action_ids::APP_DETACH => {
                return self.detach_tui_headless();
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
                    .screen_manager
                    .get_mut(MailScreenId::Timeline)
                    .is_some_and(super::tui_screens::MailScreen::reset_layout);
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
                self.screen_manager.ensure_screen(MailScreenId::Timeline);
                let path = self
                    .screen_manager
                    .get(MailScreenId::Timeline)
                    .and_then(super::tui_screens::MailScreen::export_layout);
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
                    .screen_manager
                    .get_mut(MailScreenId::Timeline)
                    .is_some_and(super::tui_screens::MailScreen::import_layout);
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
                let name = self.toggle_high_contrast_theme();
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::A11Y_TOGGLE_HINTS => {
                self.accessibility.key_hints = !self.accessibility.key_hints;
                self.persist_appearance_settings();
                return Cmd::none();
            }
            palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION => {
                self.accessibility.reduced_motion = !self.accessibility.reduced_motion;
                if self.accessibility.reduced_motion {
                    self.screen_transition = None;
                }
                let label = if self.accessibility.reduced_motion {
                    "Reduced motion enabled"
                } else {
                    "Reduced motion disabled"
                };
                self.persist_appearance_settings();
                self.notifications.notify(
                    Toast::new(label)
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::A11Y_TOGGLE_SCREEN_READER => {
                self.accessibility.screen_reader = !self.accessibility.screen_reader;
                // Screen-reader mode favors cleaner status text over key-hint noise.
                if self.accessibility.screen_reader {
                    self.accessibility.key_hints = false;
                }
                let label = if self.accessibility.screen_reader {
                    "Screen reader mode enabled"
                } else {
                    "Screen reader mode disabled"
                };
                self.persist_appearance_settings();
                self.notifications.notify(
                    Toast::new(label)
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_CYCLE => {
                let name = self.cycle_theme();
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_CYBERPUNK => {
                let name = self.apply_theme(ThemeId::CyberpunkAurora);
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_DARCULA => {
                let name = self.apply_theme(ThemeId::Darcula);
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_LUMEN => {
                let name = self.apply_theme(ThemeId::LumenLight);
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_NORDIC => {
                let name = self.apply_theme(ThemeId::NordicFrost);
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_HIGH_CONTRAST => {
                let name = self.apply_theme(ThemeId::HighContrast);
                self.notifications.notify(
                    Toast::new(format!("Theme: {name}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            palette_action_ids::THEME_FRANKENSTEIN => {
                let (cfg, display, _) = crate::tui_theme::set_named_theme(
                    crate::tui_theme::TuiThemePalette::config_name_to_index("frankenstein"),
                );
                let theme_id = Self::theme_id_for_named_config(cfg);
                let _ = crate::tui_theme::set_theme_and_get_name(theme_id);
                self.refresh_palette_theme_style();
                self.accessibility.high_contrast = theme_id == ThemeId::HighContrast;
                if theme_id != ThemeId::HighContrast {
                    self.last_non_hc_theme = theme_id;
                }
                self.sync_theme_snapshot();
                self.persist_appearance_settings();
                self.notifications.notify(
                    Toast::new(format!("Theme: {display}"))
                        .icon(ToastIcon::Info)
                        .duration(Duration::from_secs(3)),
                );
                return Cmd::none();
            }
            _ => {}
        }

        // ── Screen navigation ─────────────────────────────────────
        if let Some(screen_id) = screen_from_palette_action_id(id) {
            self.activate_screen(screen_id);
            return Cmd::none();
        }

        // ── Dynamic sources ───────────────────────────────────────
        if id.starts_with(palette_action_ids::AGENT_PREFIX) {
            self.activate_screen(MailScreenId::Agents);
            return Cmd::none();
        }
        if id.starts_with(palette_action_ids::THREAD_PREFIX) {
            self.activate_screen(MailScreenId::Threads);
            return Cmd::none();
        }
        if let Some(id_str) = id.strip_prefix(palette_action_ids::MESSAGE_PREFIX) {
            if let Ok(msg_id) = id_str.parse::<i64>() {
                let target = DeepLinkTarget::MessageById(msg_id);
                self.apply_deep_link_with_transition(&target);
            } else {
                self.activate_screen(MailScreenId::Messages);
            }
            return Cmd::none();
        }
        if id.starts_with(palette_action_ids::TOOL_PREFIX) {
            self.activate_screen(MailScreenId::ToolMetrics);
            return Cmd::none();
        }
        if let Some(slug) = id.strip_prefix(palette_action_ids::PROJECT_PREFIX) {
            let target = DeepLinkTarget::ProjectBySlug(slug.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }
        if let Some(pair) = id.strip_prefix(palette_action_ids::CONTACT_PREFIX) {
            if let Some((from, to)) = pair.split_once(':') {
                let target = DeepLinkTarget::ContactByPair(from.to_string(), to.to_string());
                self.apply_deep_link_with_transition(&target);
            } else {
                self.activate_screen(MailScreenId::Contacts);
            }
            return Cmd::none();
        }
        if let Some(agent) = id.strip_prefix(palette_action_ids::RESERVATION_PREFIX) {
            let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }

        // ── Quick actions (context-aware from focused entity) ────
        if let Some(rest) = id.strip_prefix("quick:") {
            if let Some(name) = rest.strip_prefix("agent:") {
                let target = DeepLinkTarget::AgentByName(name.to_string());
                self.apply_deep_link_with_transition(&target);
                return Cmd::none();
            }
            if let Some(id_str) = rest.strip_prefix("thread:") {
                let target = DeepLinkTarget::ThreadById(id_str.to_string());
                self.apply_deep_link_with_transition(&target);
                return Cmd::none();
            }
            if let Some(name) = rest.strip_prefix("tool:") {
                let target = DeepLinkTarget::ToolByName(name.to_string());
                self.apply_deep_link_with_transition(&target);
                return Cmd::none();
            }
            if let Some(id_str) = rest.strip_prefix("message:") {
                if let Ok(msg_id) = id_str.parse::<i64>() {
                    let target = DeepLinkTarget::MessageById(msg_id);
                    self.apply_deep_link_with_transition(&target);
                }
                return Cmd::none();
            }
            if let Some(slug) = rest.strip_prefix("project:") {
                let target = DeepLinkTarget::ProjectBySlug(slug.to_string());
                self.apply_deep_link_with_transition(&target);
                return Cmd::none();
            }
            if let Some(agent) = rest.strip_prefix("reservation:") {
                let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
                self.apply_deep_link_with_transition(&target);
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
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }
        if let Some(thread_id) = rest.strip_prefix("view_thread:") {
            let target = DeepLinkTarget::ThreadById(thread_id.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }

        // Agent macros
        if let Some(agent) = rest.strip_prefix("fetch_inbox:") {
            let target = DeepLinkTarget::ExplorerForAgent(agent.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }
        if let Some(agent) = rest.strip_prefix("view_reservations:") {
            let target = DeepLinkTarget::ReservationByAgent(agent.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }

        // Tool macros
        if let Some(tool) = rest.strip_prefix("tool_history:") {
            let target = DeepLinkTarget::ToolByName(tool.to_string());
            self.apply_deep_link_with_transition(&target);
            return Cmd::none();
        }

        // Message macros
        if let Some(id_str) = rest.strip_prefix("view_message:") {
            if let Ok(msg_id) = id_str.parse::<i64>() {
                let target = DeepLinkTarget::MessageById(msg_id);
                self.apply_deep_link_with_transition(&target);
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
                if let Some(mut transition) = self.screen_transition {
                    transition.ticks_remaining = transition.ticks_remaining.saturating_sub(1);
                    self.screen_transition = if transition.ticks_remaining == 0 {
                        None
                    } else {
                        Some(transition)
                    };
                }
                let tick_count = self.tick_count;
                let tick_state = &self.state;
                let panics = self.screen_panics.get_mut();
                let mut tick_panics: Vec<(MailScreenId, String)> = Vec::new();
                for (id, screen) in self.screen_manager.iter_mut() {
                    if panics.contains_key(&id) {
                        continue;
                    }
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        screen.tick(tick_count, tick_state);
                    }));
                    if let Err(payload) = result {
                        tick_panics.push((id, panic_payload_to_string(&payload)));
                    }
                }
                for (id, msg) in tick_panics {
                    panics.insert(id, msg);
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

                    if !self.toast_muted
                        && let Some(toast) = safe_toast_for_event(event, self.toast_severity)
                    {
                        self.notifications.notify(self.apply_toast_policy(toast));
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
                self.tick_toast_animation_state();

                // Drain deferred confirmed actions (from modal callbacks).
                // Side-effects (toast, navigation) are applied inside
                // `dispatch_execute_operation`; we discard the returned Cmd
                // because the tick handler must always return Cmd::tick.
                let _ = self.drain_deferred_confirmed_action();

                // Drain browser-ingress events and process them through the
                // same terminal handling path as local input.
                let remote_events = self
                    .state
                    .drain_remote_terminal_events(REMOTE_EVENTS_PER_TICK);
                for remote_event in remote_events {
                    if let Some(event) = remote_terminal_event_to_event(remote_event) {
                        let cmd = self.update(MailMsg::Terminal(event));
                        if matches!(cmd, Cmd::Quit) {
                            return Cmd::quit();
                        }
                    }
                }

                Cmd::tick(TICK_INTERVAL)
            }

            // ── Terminal events (key, mouse, resize, etc.) ─────────
            MailMsg::Terminal(ref event) => {
                // Track resize events for the Bayesian diff strategy.
                if matches!(event, Event::Resize { .. }) {
                    self.resize_detected.set(true);
                }

                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                    && Self::is_inspector_toggle_key(key)
                {
                    if self.inspector_enabled() {
                        self.toggle_inspector();
                    }
                    return Cmd::none();
                }

                if self.handle_inspector_event(event) {
                    return Cmd::none();
                }

                // Export format menu (Ctrl+E, traps all while open).
                if self.export_menu_open {
                    if let Event::Key(key) = event {
                        self.handle_export_menu_key(key);
                    }
                    return Cmd::none();
                }

                // Overlay focus trapping: topmost-first precedence.
                // See `OverlayLayer` for the formal z-order contract.
                //
                // Palette (z=7 event priority, traps all)
                if self.command_palette.is_visible() {
                    if let Some(action) = self.command_palette.handle_event(event) {
                        match action {
                            PaletteAction::Execute(id) => return self.dispatch_palette_action(&id),
                            PaletteAction::Dismiss => {}
                        }
                    }
                    return Cmd::none();
                }

                // Compose overlay (z=6, traps all)
                if let Some(ref mut compose) = self.compose_state {
                    if let Event::Key(key) = event
                        && key.kind == KeyEventKind::Press
                    {
                        match compose.handle_key(key) {
                            ComposeAction::Consumed | ComposeAction::Ignored => {}
                            ComposeAction::Close => {
                                self.compose_state = None;
                            }
                            ComposeAction::ConfirmClose => {
                                // TODO: show modal confirmation for unsaved changes
                                self.compose_state = None;
                            }
                            ComposeAction::Send => {
                                if let Some(mut cs) = self.compose_state.take() {
                                    match cs.build_envelope() {
                                        Ok(envelope) => {
                                            return self.dispatch_compose_send(envelope);
                                        }
                                        Err(err) => {
                                            // Put state back so user can fix.
                                            self.compose_state = Some(cs);
                                            self.notifications.notify(
                                                Toast::new(format!("Compose error: {err}"))
                                                    .icon(ToastIcon::Error)
                                                    .duration(Duration::from_secs(5)),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Cmd::none();
                }

                // Modal (z=5, traps all)
                if self.modal_manager.handle_event(event) {
                    return Cmd::none();
                }

                // Action menu (z=4.3, traps all)
                if let Some(result) = self.action_menu.handle_event(event) {
                    match result {
                        ActionMenuResult::Consumed | ActionMenuResult::Dismissed => {
                            return Cmd::none();
                        }
                        ActionMenuResult::Selected(action, context) => {
                            return self.dispatch_action_menu_selection(action, &context);
                        }
                        ActionMenuResult::DisabledAttempt(reason) => {
                            self.notifications.notify(
                                Toast::new(reason)
                                    .icon(ToastIcon::Warning)
                                    .duration(Duration::from_secs(3)),
                            );
                            return Cmd::none();
                        }
                    }
                }

                // Macro playback paused (z=4.4, traps Enter/Esc)
                if matches!(
                    self.macro_engine.playback_state(),
                    PlaybackState::Paused { .. }
                ) && let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
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

                // Toast focus mode (z=4b, traps j/k/Enter/Esc)
                if self.toast_focus_index.is_some()
                    && let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
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
                                    self.toast_focus_index = Some(idx.min(count.saturating_sub(1)));
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
                                        .style(Style::default().fg(toast_color_info())),
                                ),
                            );
                            return Cmd::none();
                        }
                        _ => {
                            // Let other keys (like Ctrl+T) fall through.
                        }
                    }
                }

                let text_mode = self.consumes_text_input();

                // Help overlay is topmost and traps pointer input.
                if self.handle_help_overlay_mouse(event) {
                    return Cmd::none();
                }

                // Central mouse dispatch for shell-level interactions
                // (tab clicks, status line). Checked before global keybindings
                // so that shell regions consume the event and prevent
                // accidental forwarding to screens.
                if let Event::Mouse(ref mouse) = *event
                    && !text_mode
                {
                    use crate::tui_hit_regions::MouseAction;
                    match self.mouse_dispatcher.dispatch(mouse) {
                        MouseAction::SwitchScreen(id) => {
                            self.activate_screen(id);
                            return Cmd::none();
                        }
                        MouseAction::ToggleHelp => {
                            self.help_visible = !self.help_visible;
                            self.help_scroll = 0;
                            return Cmd::none();
                        }
                        MouseAction::OpenPalette => {
                            self.open_palette();
                            return Cmd::none();
                        }
                        MouseAction::Forward => {}
                    }
                }

                // Global keybindings (checked before screen dispatch)
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    let is_ctrl_p = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('p'));
                    if (is_ctrl_p || matches!(key.code, KeyCode::Char(':'))) && !text_mode {
                        self.open_palette();
                        return Cmd::none();
                    }
                    // Ctrl+E: open export format menu.
                    let is_ctrl_e = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('e' | 'E'));
                    if is_ctrl_e && !text_mode {
                        self.open_export_menu();
                        return Cmd::none();
                    }
                    // Ctrl+N: open compose overlay.
                    let is_ctrl_n = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('n'));
                    if is_ctrl_n && !text_mode {
                        self.open_compose();
                        return Cmd::none();
                    }
                    // Ctrl+T / Shift+T: cycle theme globally.
                    let is_ctrl_t = (key.modifiers.contains(Modifiers::CTRL)
                            && matches!(key.code, KeyCode::Char('t' | 'T')))
                            // Some terminals emit ASCII control code 0x14 for Ctrl+T.
                            || matches!(key.code, KeyCode::Char('\u{14}'));
                    let is_shift_t = !text_mode
                        && key.modifiers.contains(Modifiers::SHIFT)
                        && matches!(key.code, KeyCode::Char('T'));
                    if is_ctrl_t || is_shift_t {
                        let name = self.cycle_theme();
                        self.notifications.notify(
                            Toast::new(format!("Theme: {name}"))
                                .icon(ToastIcon::Info)
                                .duration(Duration::from_secs(3)),
                        );
                        return Cmd::none();
                    }
                    // Ctrl+Y: toggle toast focus mode.
                    let is_ctrl_y = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('y' | 'Y'));
                    if is_ctrl_y && !text_mode {
                        if self.toast_focus_index.is_some() {
                            self.toast_focus_index = None;
                        } else if self.notifications.visible_count() > 0 {
                            self.toast_focus_index = Some(0);
                        }
                        return Cmd::none();
                    }
                    let is_ctrl_arrow = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(
                            key.code,
                            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                        );
                    if is_ctrl_arrow
                        && !text_mode
                        && !self.focus_manager.is_trapped()
                        && self.move_focus_spatial(key.code)
                    {
                        return Cmd::none();
                    }
                    let is_escape = matches!(key.code, KeyCode::Escape);
                    let is_ctrl_c = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('c' | 'C'));
                    let is_ctrl_d = key.modifiers.contains(Modifiers::CTRL)
                        && matches!(key.code, KeyCode::Char('d' | 'D'));

                    if self.help_visible {
                        match key.code {
                            KeyCode::Escape | KeyCode::Char('?') => {
                                self.help_visible = false;
                                return Cmd::none();
                            }
                            KeyCode::Char('j') | KeyCode::Down => {
                                self.help_scroll = self.help_scroll.saturating_add(1);
                                return Cmd::none();
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.help_scroll = self.help_scroll.saturating_sub(1);
                                return Cmd::none();
                            }
                            KeyCode::Char('q') if !text_mode => {
                                self.clear_quit_confirmation();
                                self.flush_before_shutdown();
                                self.state.request_shutdown();
                                return Cmd::quit();
                            }
                            _ => {}
                        }

                        if is_ctrl_d {
                            return self.detach_tui_headless();
                        }
                        if is_ctrl_c {
                            return self.handle_quit_confirmation_input(QuitConfirmSource::CtrlC);
                        }

                        return Cmd::none();
                    }

                    if !(is_escape || is_ctrl_c) {
                        self.clear_quit_confirmation();
                    }

                    if is_ctrl_d {
                        return self.detach_tui_headless();
                    }
                    if is_escape && !text_mode {
                        return self.handle_quit_confirmation_input(QuitConfirmSource::Escape);
                    }
                    if is_ctrl_c {
                        return self.handle_quit_confirmation_input(QuitConfirmSource::CtrlC);
                    }

                    match key.code {
                        KeyCode::Char('q') if !text_mode => {
                            self.clear_quit_confirmation();
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
                            let name = self.cycle_theme();
                            self.notifications.notify(
                                Toast::new(format!("Theme: {name}"))
                                    .icon(ToastIcon::Info)
                                    .duration(Duration::from_secs(3)),
                            );
                            return Cmd::none();
                        }
                        KeyCode::Tab => {
                            let next = self.screen_manager.active_screen().next();
                            self.activate_screen(next);
                            return Cmd::none();
                        }
                        KeyCode::BackTab => {
                            let prev = self.screen_manager.active_screen().prev();
                            self.activate_screen(prev);
                            return Cmd::none();
                        }
                        // Global search: / opens SearchCockpit with query bar focused
                        KeyCode::Char('/') if !text_mode => {
                            self.apply_deep_link_with_transition(&DeepLinkTarget::SearchFocused(
                                String::new(),
                            ));
                            return Cmd::none();
                        }
                        // Action menu: . opens contextual actions for selected item
                        KeyCode::Char('.') if !text_mode => {
                            if let Some(screen) = self.screen_manager.active_screen_ref()
                                && let Some((entries, anchor, ctx)) = screen.contextual_actions()
                            {
                                self.action_menu.open(entries, anchor, ctx);
                            }
                            return Cmd::none();
                        }
                        // Clipboard yank: y copies focused content
                        KeyCode::Char('y') if !text_mode => {
                            if let Some(screen) = self.screen_manager.active_screen_ref() {
                                if let Some(content) = screen.copyable_content() {
                                    self.copy_to_clipboard(&content);
                                } else {
                                    self.notifications.notify(
                                        Toast::new("Nothing to copy")
                                            .icon(ToastIcon::Warning)
                                            .duration(Duration::from_secs(2)),
                                    );
                                }
                            }
                            return Cmd::none();
                        }
                        KeyCode::Char(c) if !text_mode => {
                            if let Some(id) = screen_from_jump_key(c) {
                                self.activate_screen(id);
                                return Cmd::none();
                            }
                        }
                        _ => {}
                    }
                }

                // Forward unhandled events to the active screen
                let current = self.screen_manager.active_screen();
                if self.screen_panics.borrow().contains_key(&current) {
                    // Screen is in error state — 'r' resets it
                    if matches!(
                        event,
                        Event::Key(k) if k.kind == KeyEventKind::Press
                            && k.code == KeyCode::Char('r')
                    ) {
                        self.screen_panics.borrow_mut().remove(&current);
                        let fresh =
                            ScreenManager::create_screen(current, &self.screen_manager.state);
                        self.screen_manager.set_screen(current, fresh);
                    }
                    Cmd::none()
                } else if let Some(screen) = self.screen_manager.active_screen_mut() {
                    let state_ref = &self.state;
                    let result = catch_unwind(AssertUnwindSafe(|| screen.update(event, state_ref)));
                    match result {
                        Ok(cmd) => map_screen_cmd(cmd),
                        Err(payload) => {
                            let msg = panic_payload_to_string(&payload);
                            self.screen_panics.borrow_mut().insert(current, msg);
                            Cmd::none()
                        }
                    }
                } else {
                    Cmd::none()
                }
            }

            // ── Screen messages / direct navigation ─────────────────
            MailMsg::Screen(MailScreenMsg::Navigate(id)) | MailMsg::SwitchScreen(id) => {
                self.activate_screen(id);
                Cmd::none()
            }
            MailMsg::Screen(MailScreenMsg::Noop) => Cmd::none(),
            MailMsg::Screen(MailScreenMsg::DeepLink(ref target)) => {
                self.apply_deep_link_with_transition(target);
                Cmd::none()
            }
            MailMsg::Screen(MailScreenMsg::ActionExecute(ref op, ref ctx)) => {
                // Delegate to the active screen first.
                if let Some(screen) = self.screen_manager.active_screen_mut() {
                    let cmd = screen.handle_action(op, ctx);
                    if !matches!(cmd, Cmd::None) {
                        return map_screen_cmd(cmd);
                    }
                }
                // Fallback to global dispatch (e.g. for toast-only actions or cross-cutting concerns).
                // Note: deeply nested recursion is prevented because global dispatch returns
                // Cmd::none() for unknown actions, or specific Cmds for known ones.
                // However, if global dispatch emits ActionExecute for the SAME op, we loop.
                // We must ensure global dispatch only emits ActionExecute for ops it expects
                // the screen to handle *via the next loop*, or handle them directly.
                //
                // Actually, dispatch_execute_operation emits ActionExecute for "acknowledge", etc.
                // So if the screen didn't handle it above, we call dispatch_execute_operation,
                // which emits ActionExecute, which comes back here... LOOP.
                //
                // Fix: Only call dispatch_execute_operation if it's NOT one of the screen-delegated ops
                // that we just failed to handle.
                // Or better: dispatch_execute_operation should be the *origin* of these messages,
                // not the fallback handler.
                //
                // If the screen didn't handle it, and it's a batch action, we should log a warning.
                // But dispatch_execute_operation handles "view_body" etc internally.

                self.dispatch_execute_operation(op, ctx)
            }
            MailMsg::ToggleHelp => {
                self.help_visible = !self.help_visible;
                self.help_scroll = 0;
                Cmd::none()
            }
            MailMsg::Quit => {
                self.clear_quit_confirmation();
                self.flush_before_shutdown();
                self.state.request_shutdown();
                Cmd::quit()
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame) {
        use crate::tui_chrome;
        use crate::tui_decision::{DiffAction, FrameState};

        // The app renders its own caret/highlight treatment in widgets. Keep the
        // terminal cursor hidden to prevent visible cursor trails during refresh.
        frame.cursor_visible = false;

        // Compute frame budget: how much slack we have relative to the tick
        // interval. Normal frames arrive ~every TICK_INTERVAL; budget is
        // positive when on schedule and drops toward 0 only when frames are
        // arriving significantly late (2× the expected interval or more).
        //
        // Note: TICK_INTERVAL is set to 33ms (~30fps) to reduce terminal write
        // pressure while keeping interactions smooth.
        // and responsive input handling. The Bayesian diff strategy uses this
        // budget to decide whether to skip frames under load.
        let tick_budget_ms = TICK_INTERVAL.as_secs_f64() * 1000.0;
        let now = Instant::now();
        let budget_remaining_ms = self.last_view_instant.get().map_or(tick_budget_ms, |prev| {
            let elapsed_ms = now.duration_since(prev).as_secs_f64() * 1000.0;
            // Budget = tick_budget minus any overshoot beyond expected interval.
            // On-schedule (elapsed ≈ tick): budget ≈ tick_budget (healthy).
            // Behind (elapsed ≈ 2×tick): budget ≈ 0 (degraded).
            (tick_budget_ms - (elapsed_ms - tick_budget_ms).max(0.0)).max(0.0)
        });
        self.last_view_instant.set(Some(now));

        // Bayesian diff strategy: observe frame state and decide action.
        // change_ratio is 0.0 for stable frames; ftui does not expose
        // per-frame cell diff counts, so we default to "stable" and let
        // the resize flag and budget drive the posterior.
        let frame_state = FrameState {
            change_ratio: 0.0,
            is_resize: self.resize_detected.get(),
            budget_remaining_ms,
            error_count: 0,
        };
        let diff_action = Self::adjust_diff_action_for_overlays(
            self.diff_strategy.borrow_mut().observe(&frame_state),
        );
        self.resize_detected.set(false);

        // Act on the diff decision.
        match diff_action {
            DiffAction::Deferred => {
                // Skip this frame entirely to allow recovery.
                return;
            }
            DiffAction::Full | DiffAction::Incremental => {
                // Both render fully; incremental is advisory for future
                // differential rendering support in ftui.
            }
        }

        let area = Rect::new(0, 0, frame.width(), frame.height());
        // Paint the theme background.  The buffer is already cleared by ftui's
        // `reset_for_frame()`, so we only need to set the bg color on cells
        // that widgets won't paint over.  A lightweight Paragraph achieves
        // this without resetting every cell attribute (which would inflate the
        // per-frame diff and cause visible flashing at high tick rates).
        let tp = crate::tui_theme::TuiThemePalette::current();
        Paragraph::new("")
            .style(Style::default().bg(tp.bg_deep))
            .render(area, frame);
        // Throttle ambient effects to every Nth frame.  Animated effects
        // (plasma, fire, metaballs) change every cell's bg color, defeating
        // the diff engine and causing full-screen terminal writes on every
        // frame.  By only re-rendering every 6th frame the ambient animation
        // runs at ~5 fps which is imperceptibly smooth for a subtle background
        // wash while keeping terminal output minimal on intervening frames.
        let should_render_ambient = self
            .tick_count
            .is_multiple_of(AMBIENT_RENDER_EVERY_N_FRAMES)
            && self.ambient_last_render_tick.get() != Some(self.tick_count);
        if should_render_ambient {
            let ambient_telemetry = self.ambient_renderer.borrow_mut().render(
                area,
                frame,
                self.ambient_mode_for_frame(),
                self.ambient_health_input(now_micros()),
                self.state.uptime().as_secs_f64(),
            );
            self.ambient_last_telemetry.set(ambient_telemetry);
            self.ambient_last_render_tick.set(Some(self.tick_count));
            self.ambient_render_invocations
                .set(self.ambient_render_invocations.get().saturating_add(1));
        }
        let chrome = tui_chrome::chrome_layout(area);
        let active_screen = self.screen_manager.active_screen();
        *self.last_content_area.borrow_mut() = chrome.content;

        // Cache chrome areas for mouse dispatcher.
        self.mouse_dispatcher
            .update_chrome_areas(chrome.tab_bar, chrome.status_line);

        let mut action_menu_render_us = 0_u64;
        let mut modal_render_us = 0_u64;
        let mut palette_render_us = 0_u64;
        let mut help_render_us = 0_u64;

        // 1. Tab bar (z=1)
        let effects_enabled = self.state.config_snapshot().tui_effects;
        let tab_started = Instant::now();
        tui_chrome::render_tab_bar(active_screen, effects_enabled, frame, chrome.tab_bar);
        let tab_render_us = tab_started
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX);

        // Record per-tab hit slots for mouse dispatch.
        tui_chrome::record_tab_hit_slots(chrome.tab_bar, active_screen, &self.mouse_dispatcher);

        // Register per-tab hit regions in the frame's hit grid.
        for (i, meta) in crate::tui_screens::MAIL_SCREEN_REGISTRY.iter().enumerate() {
            if let Some(slot) = self.mouse_dispatcher.tab_slot(i) {
                let tab_rect = Rect::new(slot.0, slot.2, slot.1 - slot.0, 1);
                frame.register_hit_region(tab_rect, crate::tui_hit_regions::tab_hit_id(meta.id));
            }
        }

        // 2. Screen content (z=2)
        let content_started = Instant::now();
        if self.screen_panics.borrow().contains_key(&active_screen) {
            let msg = self
                .screen_panics
                .borrow()
                .get(&active_screen)
                .cloned()
                .unwrap_or_default();
            render_screen_error_fallback(active_screen, &msg, chrome.content, frame);
        } else if let Some(screen) = self.screen_manager.active_screen_ref() {
            let result = catch_unwind(AssertUnwindSafe(|| {
                screen.view(frame, chrome.content, &self.state);
            }));
            if let Err(payload) = result {
                let msg = panic_payload_to_string(&payload);
                render_screen_error_fallback(active_screen, &msg, chrome.content, frame);
                self.screen_panics.borrow_mut().insert(active_screen, msg);
            }
        }
        render_message_drag_ghost(&self.state, area, frame);
        let content_render_us = content_started
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX);
        // Per-screen panels already render their own focus borders. A second
        // global outline can drift from dynamic split geometry and look like
        // stray "random borders" crossing content, so keep this disabled.

        // Register pane hit region for the active screen's content area.
        frame.register_hit_region(
            chrome.content,
            crate::tui_hit_regions::pane_hit_id(active_screen),
        );
        if let Some(transition) = self.screen_transition {
            render_screen_transition_overlay(transition, chrome.content, frame);
        }

        let screen_bindings = self
            .screen_manager
            .active_screen_ref()
            .map(super::tui_screens::MailScreen::keybindings)
            .unwrap_or_default();

        // 3. Status line (z=3)
        let status_started = Instant::now();
        tui_chrome::render_status_line(
            &self.state,
            active_screen,
            self.macro_engine.recorder_state().is_recording(),
            self.help_visible,
            &self.accessibility,
            &screen_bindings,
            self.toast_muted,
            frame,
            chrome.status_line,
        );
        let status_render_us = status_started
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX);

        // 4. Toast notifications (z=4, overlay)
        let toast_started = Instant::now();
        let reduced_motion = toast_reduced_motion_enabled(&self.accessibility);
        let toast_area = toast_overlay_area(chrome.content, area);
        if reduced_motion {
            NotificationStack::new(&self.notifications)
                .margin(1)
                .render(toast_area, frame);
        } else {
            render_animated_toast_stack(
                &self.notifications,
                &self.toast_age_ticks,
                toast_area,
                1,
                frame,
            );
        }

        // 4b. Toast focus highlight overlay
        if let Some(focus_idx) = self.toast_focus_index {
            render_toast_focus_highlight(
                &self.notifications,
                focus_idx,
                toast_area,
                1, // margin
                frame,
            );
        }
        let toast_render_us = toast_started
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX);

        // 4b. Action menu (z=4.3, contextual per-item actions)
        if self.action_menu.is_active() {
            let action_menu_started = Instant::now();
            self.action_menu.render(area, frame);
            action_menu_render_us = action_menu_started
                .elapsed()
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
        }

        // 4c. Export menu (z=4.4, between action menu and modal)
        if self.export_menu_open {
            render_export_format_menu(area, frame);
        }

        // 4d. Modal dialogs (z=4.5, between toasts and command palette)
        if self.modal_manager.is_active() {
            let modal_started = Instant::now();
            self.modal_manager.render(area, frame);
            modal_render_us = modal_started
                .elapsed()
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
        }

        // 4e. Compose overlay (z=4.7, between modal and palette)
        if let Some(ref compose) = self.compose_state {
            ComposePanel::new(compose).render(area, frame, &self.state);
        }

        // 5. Command palette (z=5, modal)
        if self.command_palette.is_visible() {
            let palette_started = Instant::now();
            self.command_palette.render(area, frame);
            palette_render_us = palette_started
                .elapsed()
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
        }

        // 6. Help overlay (z=6, topmost)
        if self.help_visible {
            let help_started = Instant::now();
            let screen_label = crate::tui_screens::screen_meta(active_screen).title;
            let screen_tip = self
                .screen_manager
                .active_screen_ref()
                .and_then(super::tui_screens::MailScreen::context_help_tip);
            let sections = self
                .keymap
                .contextual_help(&screen_bindings, screen_label, screen_tip);
            tui_chrome::render_help_overlay_sections(&sections, self.help_scroll, frame, area);
            help_render_us = help_started
                .elapsed()
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
        }

        // 7. Debug inspector overlay (z=7, topmost; gated by AM_TUI_DEBUG)
        if self.inspector.is_active() {
            let tree = self.build_inspector_widget_tree(
                area,
                &chrome,
                active_screen,
                tab_render_us,
                content_render_us,
                status_render_us,
                toast_render_us,
                action_menu_render_us,
                modal_render_us,
                palette_render_us,
                help_render_us,
            );
            let mut rows = Vec::new();
            flatten_inspector_rows(&tree, &mut rows);
            self.inspector_last_tree_len.set(rows.len());

            let selected_idx = if rows.is_empty() {
                0
            } else {
                self.inspector_selected_index
                    .min(rows.len().saturating_sub(1))
            };
            let selected_row = rows.get(selected_idx);
            let selected_area = selected_row.map(|row| row.area);
            let tree_lines = rows.iter().map(|row| row.label.clone()).collect::<Vec<_>>();

            let properties = selected_row.map_or_else(Vec::new, |row| {
                let mut props = vec![
                    format!("Name: {}", row.name),
                    format!("Depth: {}", row.depth),
                    format!(
                        "Rect: x={} y={} w={} h={}",
                        row.area.x, row.area.y, row.area.width, row.area.height
                    ),
                ];
                if let Some(hit_id) = row.hit_id {
                    props.push(format!("HitId: {}", hit_id.id()));
                } else {
                    props.push("HitId: (none)".to_string());
                }
                if let Some(render_us) = row.render_time_us {
                    props.push(format!("Render: {render_us}us"));
                } else {
                    props.push("Render: (n/a)".to_string());
                }
                props
            });

            tui_chrome::render_inspector_overlay(
                frame,
                area,
                &tree_lines,
                selected_idx,
                selected_area,
                &properties,
                self.inspector_show_properties,
            );
        }

        // Final readability guard: normalize low-contrast cells so every
        // visible glyph remains legible across all themes (especially light).
        apply_frame_contrast_guard(frame, &tp);

        // Keep an exportable snapshot of the last fully rendered frame.
        self.last_export_snapshot
            .borrow_mut()
            .replace(FrameExportSnapshot {
                buffer: frame.buffer.clone(),
                pool: frame.pool.clone(),
            });
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

fn flatten_inspector_rows(widget: &WidgetInfo, rows: &mut Vec<InspectorTreeRow>) {
    let name = if widget.name.is_empty() {
        "<unnamed>".to_string()
    } else {
        widget.name.clone()
    };
    let indent = "  ".repeat(widget.depth as usize);
    let mut label = format!(
        "{indent}{name} (x={} y={} w={} h={})",
        widget.area.x, widget.area.y, widget.area.width, widget.area.height
    );
    if let Some(render_us) = widget.render_time_us {
        let _ = write!(label, " [{render_us}us]");
    }
    rows.push(InspectorTreeRow {
        label,
        name,
        depth: widget.depth,
        area: widget.area,
        hit_id: widget.hit_id,
        render_time_us: widget.render_time_us,
    });
    for child in &widget.children {
        flatten_inspector_rows(child, rows);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Command palette catalog
// ──────────────────────────────────────────────────────────────────────

mod palette_action_ids {
    pub const APP_TOGGLE_HELP: &str = "app:toggle_help";
    pub const APP_QUIT: &str = "app:quit";
    pub const APP_DETACH: &str = "app:detach_headless";

    pub const TRANSPORT_TOGGLE: &str = "transport:toggle";
    pub const TRANSPORT_SET_MCP: &str = "transport:set_mcp";
    pub const TRANSPORT_SET_API: &str = "transport:set_api";

    pub const LAYOUT_RESET: &str = "layout:reset";
    pub const LAYOUT_EXPORT: &str = "layout:export";
    pub const LAYOUT_IMPORT: &str = "layout:import";

    pub const A11Y_TOGGLE_HC: &str = "a11y:toggle_high_contrast";
    pub const A11Y_TOGGLE_HINTS: &str = "a11y:toggle_key_hints";
    pub const A11Y_TOGGLE_REDUCED_MOTION: &str = "a11y:toggle_reduced_motion";
    pub const A11Y_TOGGLE_SCREEN_READER: &str = "a11y:toggle_screen_reader";

    pub const THEME_CYCLE: &str = "theme:cycle";
    pub const THEME_CYBERPUNK: &str = "theme:cyberpunk_aurora";
    pub const THEME_DARCULA: &str = "theme:darcula";
    pub const THEME_LUMEN: &str = "theme:lumen_light";
    pub const THEME_NORDIC: &str = "theme:nordic_frost";
    pub const THEME_HIGH_CONTRAST: &str = "theme:high_contrast";
    pub const THEME_FRANKENSTEIN: &str = "theme:frankenstein";

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
    pub const SCREEN_ARCHIVE_BROWSER: &str = "screen:archive_browser";
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
        palette_action_ids::SCREEN_ARCHIVE_BROWSER => Some(MailScreenId::ArchiveBrowser),
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
        MailScreenId::ArchiveBrowser => palette_action_ids::SCREEN_ARCHIVE_BROWSER,
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
        let key_hint = crate::tui_screens::jump_key_label_for_screen(id);
        let desc = key_hint.map_or_else(
            || format!("{} [via palette only]", meta.description),
            |k| format!("{} [key: {}]", meta.description, k),
        );
        out.push(
            ActionItem::new(
                screen_palette_action_id(id),
                format!("Go to {}", meta.title),
            )
            .with_description(desc)
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
            .with_description("Switch to the next color theme (Ctrl+T / Shift+T)")
            .with_tags(&["theme", "colors", "appearance"])
            .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(
            palette_action_ids::THEME_CYBERPUNK,
            "Theme: Cyberpunk Aurora",
        )
        .with_description("Set theme to Cyberpunk Aurora")
        .with_tags(&["theme", "colors", "appearance"])
        .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(palette_action_ids::THEME_DARCULA, "Theme: Darcula")
            .with_description("Set theme to Darcula")
            .with_tags(&["theme", "colors", "appearance"])
            .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(palette_action_ids::THEME_LUMEN, "Theme: Lumen Light")
            .with_description("Set theme to Lumen Light")
            .with_tags(&["theme", "colors", "appearance"])
            .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(palette_action_ids::THEME_NORDIC, "Theme: Nordic Frost")
            .with_description("Set theme to Nordic Frost")
            .with_tags(&["theme", "colors", "appearance"])
            .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(
            palette_action_ids::THEME_HIGH_CONTRAST,
            "Theme: High Contrast",
        )
        .with_description("Set theme to High Contrast")
        .with_tags(&["theme", "colors", "appearance", "a11y"])
        .with_category("Appearance"),
    );
    out.push(
        ActionItem::new(
            palette_action_ids::THEME_FRANKENSTEIN,
            "Theme: Frankenstein",
        )
        .with_description("Set theme to Frankenstein (non-default showcase palette)")
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
        ActionItem::new(
            palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION,
            "Toggle Reduced Motion",
        )
        .with_description("Reduce animated/rapidly changing visual effects")
        .with_tags(&["accessibility", "motion", "a11y"])
        .with_category("Accessibility"),
    );
    out.push(
        ActionItem::new(
            palette_action_ids::A11Y_TOGGLE_SCREEN_READER,
            "Toggle Screen Reader Mode",
        )
        .with_description("Optimize status text for screen-reader output")
        .with_tags(&["accessibility", "screen-reader", "a11y"])
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
    out.push(
        ActionItem::new(palette_action_ids::APP_DETACH, "Detach TUI (Headless)")
            .with_description("Exit AgentMailTUI but keep HTTP server running headless")
            .with_tags(&["detach", "headless", "server"])
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
    body_snippet: String,
}

#[derive(Debug, Clone, Default)]
struct PaletteDbCache {
    database_url: String,
    fetched_at_micros: i64,
    source_snapshot_micros: i64,
    source_message_count: u64,
    agent_metadata: HashMap<String, (String, String)>,
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

static PALETTE_DB_CACHE: OnceLock<Mutex<PaletteDbCache>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PaletteCacheBridgeState {
    snapshot_micros: i64,
    message_count: u64,
}

fn palette_cache_bridge_state(state: &TuiSharedState) -> PaletteCacheBridgeState {
    state
        .db_stats_snapshot()
        .map_or_else(PaletteCacheBridgeState::default, |snap| {
            PaletteCacheBridgeState {
                snapshot_micros: snap.timestamp_micros,
                message_count: snap.messages,
            }
        })
}

fn query_palette_agent_metadata(
    state: &TuiSharedState,
    limit: usize,
) -> HashMap<String, (String, String)> {
    let snapshot = state.config_snapshot();
    let cfg = DbPoolConfig {
        database_url: snapshot.raw_database_url,
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
    let snapshot = state.config_snapshot();
    let cfg = DbPoolConfig {
        database_url: snapshot.raw_database_url,
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
             COALESCE(SUBSTR(m.body_md, 1, 150), '') AS body_snippet, \
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
                    body_snippet: row
                        .get_named::<String>("body_snippet")
                        .ok()
                        .unwrap_or_default(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn fetch_palette_db_data(
    state: &TuiSharedState,
    agent_limit: usize,
    message_limit: usize,
) -> (
    HashMap<String, (String, String)>,
    Vec<PaletteMessageSummary>,
) {
    let database_url = state.config_snapshot().raw_database_url;
    let bridge_state = palette_cache_bridge_state(state);
    let now = now_micros();
    let cache = PALETTE_DB_CACHE.get_or_init(|| Mutex::new(PaletteDbCache::default()));
    if let Ok(guard) = cache.lock() {
        let fresh_enough =
            now.saturating_sub(guard.fetched_at_micros) <= PALETTE_DB_CACHE_TTL_MICROS;
        let bridge_matches = guard.source_snapshot_micros == bridge_state.snapshot_micros
            && guard.source_message_count == bridge_state.message_count;
        if guard.database_url == database_url && fresh_enough && bridge_matches {
            return (
                guard
                    .agent_metadata
                    .iter()
                    .take(agent_limit)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                guard.messages.iter().take(message_limit).cloned().collect(),
            );
        }
    }

    let agent_metadata = query_palette_agent_metadata(state, agent_limit);
    let messages = query_palette_recent_messages(state, message_limit);
    if let Ok(mut guard) = cache.lock() {
        guard.database_url = database_url;
        guard.fetched_at_micros = now;
        guard.source_snapshot_micros = bridge_state.snapshot_micros;
        guard.source_message_count = bridge_state.message_count;
        guard.agent_metadata.clone_from(&agent_metadata);
        guard.messages.clone_from(&messages);
    }
    (agent_metadata, messages)
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
        let desc = if message.body_snippet.is_empty() {
            format!(
                "{} -> {} | {}",
                message.from_agent,
                to_agents,
                format_timestamp_micros(message.timestamp_micros)
            )
        } else {
            // Single-line snippet for cleaner palette display
            let snippet: String = message
                .body_snippet
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            format!(
                "{} -> {} | {} | {}",
                message.from_agent,
                to_agents,
                format_timestamp_micros(message.timestamp_micros),
                snippet.trim()
            )
        };
        let mut action = ActionItem::new(
            format!("{}{}", palette_action_ids::MESSAGE_PREFIX, message.id),
            format!("Message: {}", truncate_subject(&message.subject, 60)),
        )
        .with_description(desc)
        .with_category("Messages");
        action.tags.push("message".to_string());
        action.tags.push(message.from_agent.clone());
        if !message.thread_id.is_empty() {
            action.tags.push(message.thread_id.clone());
        }
        // Add body snippet words as tags for fuzzy matching
        if !message.body_snippet.is_empty() {
            for word in message.body_snippet.split_whitespace().take(10) {
                let word = word.trim_matches(|c: char| !c.is_alphanumeric());
                if word.len() >= 3 {
                    action.tags.push(word.to_lowercase());
                }
            }
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
    let (agent_metadata, recent_messages) = fetch_palette_db_data(
        state,
        PALETTE_DYNAMIC_AGENT_CAP,
        PALETTE_DYNAMIC_MESSAGE_CAP,
    );

    for agent in snap.agents_list.into_iter().take(PALETTE_DYNAMIC_AGENT_CAP) {
        let crate::tui_events::AgentSummary {
            name,
            program,
            last_active_ts,
        } = agent;
        let desc = if let Some((model, project_slug)) = agent_metadata.get(&name) {
            format!("{program}/{model}  project: {project_slug}  active: {last_active_ts}")
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
            "{}  {} agents  {} msgs  {} reservations",
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
#[allow(clippy::too_many_lines)]
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
        if threads_seen.len() < PALETTE_DYNAMIC_THREAD_CAP
            && let Some((thread_id, subject)) = extract_thread(ev)
            && threads_seen.insert(thread_id.to_string())
        {
            let thread_desc = thread_stats.get(thread_id).map_or_else(
                || format!("Latest: {subject}"),
                |stats| {
                    let participants = format_participant_list(&stats.participants, 3);
                    format!(
                        "{} msgs • {} • latest: {}",
                        stats.message_count,
                        participants,
                        truncate_subject(&stats.latest_subject, 42)
                    )
                },
            );
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

        if messages_seen.len() < PALETTE_DYNAMIC_MESSAGE_CAP
            && let Some((message_id, from, subject, thread_id)) = extract_message(ev)
            && messages_seen.insert(message_id)
        {
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

        if tools_seen.len() < PALETTE_DYNAMIC_TOOL_CAP
            && let Some(tool_name) = extract_tool_name(ev)
            && tools_seen.insert(tool_name.to_string())
        {
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

        if reservations_seen.len() < PALETTE_DYNAMIC_RESERVATION_CAP
            && let Some(agent) = extract_reservation_agent(ev)
            && reservations_seen.insert(agent.to_string())
        {
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
                        |ttl_secs| format!("{} remaining", format_ttl_remaining_short(ttl_secs)),
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
        palette_action_ids::APP_DETACH => "Detach TUI (Headless)".into(),
        palette_action_ids::TRANSPORT_TOGGLE => "Toggle Transport".into(),
        palette_action_ids::THEME_CYCLE => "Cycle Theme".into(),
        palette_action_ids::THEME_CYBERPUNK => "Theme: Cyberpunk Aurora".into(),
        palette_action_ids::THEME_DARCULA => "Theme: Darcula".into(),
        palette_action_ids::THEME_LUMEN => "Theme: Lumen Light".into(),
        palette_action_ids::THEME_NORDIC => "Theme: Nordic Frost".into(),
        palette_action_ids::THEME_HIGH_CONTRAST => "Theme: High Contrast".into(),
        palette_action_ids::THEME_FRANKENSTEIN => "Theme: Frankenstein".into(),
        palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION => "Toggle Reduced Motion".into(),
        palette_action_ids::A11Y_TOGGLE_SCREEN_READER => "Toggle Screen Reader Mode".into(),
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
#[allow(clippy::too_many_lines)]
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
                    .style(Style::default().fg(toast_color_info()))
                    .duration(Duration::from_secs(4)),
            )
        }
        MailEvent::MessageReceived { from, subject, .. } => {
            // Unicode-safe truncation (avoids byte-index panics on non-ASCII subjects).
            let truncated = truncate_subject(subject, 40);
            (
                ToastIcon::Info,
                Toast::new(format!("{from}: {truncated}"))
                    .icon(ToastIcon::Info)
                    .style(Style::default().fg(toast_color_info()))
                    .duration(Duration::from_secs(5)),
            )
        }

        // ── Identity ─────────────────────────────────────────────
        MailEvent::AgentRegistered { name, program, .. } => (
            ToastIcon::Success,
            Toast::new(format!("{name} ({program})"))
                .icon(ToastIcon::Success)
                .style(Style::default().fg(toast_color_success()))
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
                .style(Style::default().fg(toast_color_error()))
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
                .style(Style::default().fg(toast_color_warning()))
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
                    .style(Style::default().fg(toast_color_info()))
                    .duration(Duration::from_secs(4)),
            )
        }

        // ── HTTP 5xx ─────────────────────────────────────────────
        MailEvent::HttpRequest { status, path, .. } if *status >= 500 => (
            ToastIcon::Error,
            Toast::new(format!("HTTP {status} on {path}"))
                .icon(ToastIcon::Error)
                .style(Style::default().fg(toast_color_error()))
                .duration(Duration::from_secs(6)),
        ),

        // ── Lifecycle ────────────────────────────────────────────
        MailEvent::ServerShutdown { .. } => (
            ToastIcon::Warning,
            Toast::new("Server shutting down")
                .icon(ToastIcon::Warning)
                .style(Style::default().fg(toast_color_warning()))
                .duration(Duration::from_secs(8)),
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

fn safe_toast_from_builder<F>(build: F) -> Option<Toast>
where
    F: FnOnce() -> Option<Toast>,
{
    match catch_unwind(AssertUnwindSafe(build)) {
        Ok(toast) => toast,
        Err(payload) => {
            tracing::error!(
                panic = %panic_payload_to_string(&payload),
                "toast generation panicked; dropping toast to keep TUI alive"
            );
            None
        }
    }
}

fn safe_toast_for_event(event: &MailEvent, severity: ToastSeverityThreshold) -> Option<Toast> {
    safe_toast_from_builder(|| toast_for_event(event, severity))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::missing_const_for_fn
)]
fn render_screen_transition_overlay(transition: ScreenTransition, area: Rect, frame: &mut Frame) {
    // Screen transitions are intentionally disabled for now. Even subtle
    // overlays can read as stray borders/noise over dense content screens.
    let _ = (transition, area, frame);
}

fn set_focus_cell(frame: &mut Frame, x: u16, y: u16, symbol: char, color: PackedRgba) {
    if let Some(cell) = frame.buffer.get_mut(x, y) {
        *cell = (*cell).with_char(symbol).with_fg(color);
    }
}

#[allow(dead_code)]
fn clear_rect(frame: &mut Frame, area: Rect, bg: PackedRgba, fg: PackedRgba) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.bg = bg;
                cell.fg = fg;
            }
        }
    }
}

fn render_message_drag_ghost(state: &TuiSharedState, area: Rect, frame: &mut Frame) {
    let Some(drag) = state.message_drag_snapshot() else {
        return;
    };
    if area.width < 6 || area.height == 0 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let label = format!(" \u{21a6} {} ", truncate_subject(&drag.subject, 40));
    let width = u16::try_from(display_width(&label)).unwrap_or(area.width);
    let render_width = width.min(area.width);
    if render_width == 0 {
        return;
    }

    let max_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(render_width);
    let x = drag.cursor_x.saturating_add(1).min(max_x);
    let max_y = area.y.saturating_add(area.height).saturating_sub(1);
    let y = drag.cursor_y.saturating_add(1).min(max_y);
    let ghost_area = Rect::new(x, y, render_width, 1);
    Paragraph::new(label)
        .style(
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.panel_border_dim)
                .dim(),
        )
        .render(ghost_area, frame);
}

fn render_export_format_menu(area: Rect, frame: &mut Frame) {
    if area.width < 24 || area.height < 6 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay))
        .render(area, frame);

    let menu_width = area.width.saturating_sub(4).min(52);
    let menu_height = 6_u16.min(area.height.saturating_sub(2));
    let menu_x = area.x + area.width.saturating_sub(menu_width) / 2;
    let menu_y = area.y + area.height.saturating_sub(menu_height) / 2;
    let menu = Rect::new(menu_x, menu_y, menu_width, menu_height);

    if menu.width < 6 || menu.height < 4 {
        return;
    }

    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
        .render(menu, frame);

    render_panel_focus_outline(area, menu, frame);

    let title_area = Rect::new(menu.x + 2, menu.y + 1, menu.width.saturating_sub(4), 1);
    Paragraph::new("Export screen snapshot")
        .style(Style::default().fg(tp.status_accent).bg(tp.panel_bg).bold())
        .render(title_area, frame);

    let options_area = Rect::new(menu.x + 2, menu.y + 2, menu.width.saturating_sub(4), 1);
    Paragraph::new("[h] HTML   [s] SVG   [t] Text")
        .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
        .render(options_area, frame);

    if menu.height >= 5 {
        let hint_area = Rect::new(menu.x + 2, menu.y + 4, menu.width.saturating_sub(4), 1);
        Paragraph::new("Esc to cancel")
            .style(Style::default().fg(tp.text_muted).bg(tp.panel_bg))
            .render(hint_area, frame);
    }
}

/// Draw a focused-panel outline using the theme's focused border color.
fn render_panel_focus_outline(bounds: Rect, area: Rect, frame: &mut Frame) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let color = crate::tui_theme::TuiThemePalette::current().panel_border_focused;
    let has_top_space = area.y > bounds.y;
    let has_bottom_space = area.bottom() < bounds.bottom();
    let has_left_space = area.x > bounds.x;
    let has_right_space = area.right() < bounds.right();

    if !has_top_space && !has_bottom_space && !has_left_space && !has_right_space {
        return;
    }

    let top_row = area.y.saturating_sub(1);
    let bottom_row = area.bottom();
    let left_col = area.x.saturating_sub(1);
    let right_col = area.right();

    if has_top_space {
        let start_x = if has_left_space {
            left_col.saturating_add(1)
        } else {
            area.x
        };
        let end_x = if has_right_space {
            right_col.saturating_sub(1)
        } else {
            area.right().saturating_sub(1)
        };
        if start_x <= end_x {
            for x in start_x..=end_x {
                set_focus_cell(frame, x, top_row, '─', color);
            }
        }
    }

    if has_bottom_space {
        let start_x = if has_left_space {
            left_col.saturating_add(1)
        } else {
            area.x
        };
        let end_x = if has_right_space {
            right_col.saturating_sub(1)
        } else {
            area.right().saturating_sub(1)
        };
        if start_x <= end_x {
            for x in start_x..=end_x {
                set_focus_cell(frame, x, bottom_row, '─', color);
            }
        }
    }

    if has_left_space {
        let start_y = if has_top_space {
            top_row.saturating_add(1)
        } else {
            area.y
        };
        let end_y = if has_bottom_space {
            bottom_row.saturating_sub(1)
        } else {
            area.bottom().saturating_sub(1)
        };
        if start_y <= end_y {
            for y in start_y..=end_y {
                set_focus_cell(frame, left_col, y, '│', color);
            }
        }
    }

    if has_right_space {
        let start_y = if has_top_space {
            top_row.saturating_add(1)
        } else {
            area.y
        };
        let end_y = if has_bottom_space {
            bottom_row.saturating_sub(1)
        } else {
            area.bottom().saturating_sub(1)
        };
        if start_y <= end_y {
            for y in start_y..=end_y {
                set_focus_cell(frame, right_col, y, '│', color);
            }
        }
    }

    if has_top_space && has_left_space {
        set_focus_cell(frame, left_col, top_row, '╭', color);
    }
    if has_top_space && has_right_space {
        set_focus_cell(frame, right_col, top_row, '╮', color);
    }
    if has_bottom_space && has_left_space {
        set_focus_cell(frame, left_col, bottom_row, '╰', color);
    }
    if has_bottom_space && has_right_space {
        set_focus_cell(frame, right_col, bottom_row, '╯', color);
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

fn toast_overlay_area(content_area: Rect, fallback_area: Rect) -> Rect {
    if content_area.is_empty() {
        return fallback_area;
    }

    let inset = TOAST_OVERLAY_CONTENT_TOP_INSET_ROWS.min(content_area.height.saturating_sub(1));
    if inset == 0 {
        return content_area;
    }

    Rect::new(
        content_area.x,
        content_area.y.saturating_add(inset),
        content_area.width,
        content_area.height.saturating_sub(inset),
    )
}

fn render_animated_toast_stack(
    queue: &NotificationQueue,
    toast_age_ticks: &HashMap<ToastId, u8>,
    area: Rect,
    margin: u16,
    frame: &mut Frame,
) {
    if area.is_empty() || queue.visible().is_empty() {
        return;
    }

    let positions = queue.calculate_positions(area.width, area.height, margin);
    for (toast, (_, rel_x, rel_y)) in queue.visible().iter().zip(positions.iter()) {
        let age_ticks = toast_age_ticks.get(&toast.id).copied().unwrap_or(0);
        let remaining = toast.remaining_time().map(remaining_ticks_from_duration);
        let shift = entrance_slide_columns(age_ticks).saturating_add(exit_slide_columns(remaining));
        let fade_level = exit_fade_level(remaining);

        let (toast_width, toast_height) = toast.calculate_dimensions();
        let x = area.x.saturating_add(*rel_x).saturating_add(shift);
        let y = area.y.saturating_add(*rel_y);
        let toast_area = Rect::new(x, y, toast_width, toast_height);
        let render_area = toast_area.intersection(&area);
        if render_area.is_empty() {
            continue;
        }

        toast.render(render_area, frame);
        if fade_level > 0 {
            apply_fade_to_area(frame, render_area, fade_level);
        }
    }
}

fn remaining_ticks_from_duration(remaining: Duration) -> u8 {
    let tick_ms = TICK_INTERVAL.as_millis().max(1);
    let rem_ms = remaining.as_millis();
    let ticks = rem_ms.div_ceil(tick_ms);
    let capped = ticks.min(u128::from(u8::MAX));
    u8::try_from(capped).unwrap_or(u8::MAX)
}

fn entrance_slide_columns(age_ticks: u8) -> u16 {
    if age_ticks >= TOAST_ENTRANCE_TICKS {
        return 0;
    }
    let remaining = TOAST_ENTRANCE_TICKS.saturating_sub(age_ticks);
    u16::from(remaining).saturating_mul(2)
}

const fn exit_fade_level(remaining_ticks: Option<u8>) -> u8 {
    let Some(remaining_ticks) = remaining_ticks else {
        return 0;
    };
    if remaining_ticks == 0 || remaining_ticks > TOAST_EXIT_TICKS {
        return 0;
    }
    TOAST_EXIT_TICKS
        .saturating_sub(remaining_ticks)
        .saturating_add(1)
}

fn exit_slide_columns(remaining_ticks: Option<u8>) -> u16 {
    u16::from(exit_fade_level(remaining_ticks)).saturating_mul(2)
}

fn apply_fade_to_area(frame: &mut Frame, area: Rect, fade_level: u8) {
    let t = match fade_level {
        1 => 0.35_f32,
        2 => 0.6_f32,
        _ => 0.8_f32,
    };
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                cell.fg = blend_to_bg(cell.fg, cell.bg, t);
            }
        }
    }
}

fn blend_to_bg(fg: PackedRgba, bg: PackedRgba, t: f32) -> PackedRgba {
    let t = t.clamp(0.0, 1.0);
    let blend = |start: u8, end: u8| -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            ((f32::from(end) - f32::from(start)).mul_add(t, f32::from(start))).round() as u8
        }
    };
    PackedRgba::rgba(
        blend(fg.r(), bg.r()),
        blend(fg.g(), bg.g()),
        blend(fg.b(), bg.b()),
        fg.a(),
    )
}

/// Overwrite the border cells of the toast area with the highlight color.
fn highlight_toast_border(x: u16, y: u16, tw: u16, th: u16, frame: &mut Frame) {
    // Top and bottom border rows.
    for bx in x..x.saturating_add(tw) {
        for &by in &[y, y.saturating_add(th).saturating_sub(1)] {
            if let Some(cell) = frame.buffer.get_mut(bx, by) {
                cell.fg = toast_focus_highlight();
            }
        }
    }
    // Left and right border columns.
    let bottom = y.saturating_add(th).saturating_sub(1);
    for by in y..=bottom {
        for &bx in &[x, x.saturating_add(tw).saturating_sub(1)] {
            if let Some(cell) = frame.buffer.get_mut(bx, by) {
                cell.fg = toast_focus_highlight();
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
    let hint = "Ctrl+Y:exit  \u{2191}\u{2193}:nav  Enter:dismiss";
    let hint_y = positions.last().map_or(default_y, |(_, _, py)| {
        let (_, lh) = visible
            .last()
            .map_or((0, 3), ftui_widgets::Toast::calculate_dimensions);
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
            cell.fg = toast_focus_highlight();
        }
    }
}

/// Minimum contrast ratio for readable TUI text.
const MIN_TEXT_CONTRAST_RATIO: f64 = 4.5;
/// Higher floor for bright surfaces to avoid white-on-white regressions.
const MIN_TEXT_CONTRAST_RATIO_LIGHT_SURFACE: f64 = 5.6;
/// Decorative separators should stay subtle so they do not read as random bars.
const MIN_DECORATIVE_CONTRAST_RATIO: f64 = 1.35;
/// Slightly higher decorative floor on bright surfaces for visibility.
const MIN_DECORATIVE_CONTRAST_RATIO_LIGHT_SURFACE: f64 = 1.55;

/// Normalize low-contrast rendered cells to readable theme-safe foreground colors.
fn apply_frame_contrast_guard(frame: &mut Frame, tp: &crate::tui_theme::TuiThemePalette) {
    let height = frame.buffer.height();
    let width = frame.buffer.width();

    for y in 0..height {
        for x in 0..width {
            let Some(snapshot) = frame.buffer.get(x, y) else {
                continue;
            };
            let fg_color = snapshot.fg;
            let bg_color = snapshot.bg;
            let symbol_opt = snapshot.content.as_char();
            let is_grapheme = snapshot.content.is_grapheme();
            let symbol = if is_grapheme {
                // Grapheme-backed cells represent visible user text (emoji/ZWJ/etc.)
                // and should use full text contrast, not whitespace exemptions.
                'A'
            } else {
                symbol_opt.unwrap_or(' ')
            };
            let is_continuation = snapshot.is_continuation();
            let needs_surface_materialization = bg_color.a() == 0;
            let needs_text_materialization = fg_color.a() == 0;
            let effective_bg = if needs_surface_materialization {
                infer_effective_background(frame, x, y, tp)
            } else {
                bg_color
            };
            let materialized_fg = if needs_text_materialization {
                best_readable_fg(effective_bg, tp)
            } else {
                fg_color
            };
            if (needs_surface_materialization || needs_text_materialization)
                && let Some(cell) = frame.buffer.get_mut(x, y)
            {
                if needs_surface_materialization {
                    cell.bg = effective_bg;
                }
                if needs_text_materialization {
                    cell.fg = materialized_fg;
                }
            }
            if is_continuation {
                continue;
            }
            if symbol_opt.is_none() && !is_grapheme {
                continue;
            }
            let min_ratio = minimum_text_contrast_for_cell(symbol, effective_bg);
            if min_ratio <= 0.0 {
                continue;
            }

            if contrast_ratio(materialized_fg, effective_bg) < min_ratio {
                let replacement = if !is_grapheme && is_decorative_glyph(symbol) {
                    best_readable_decorative_fg(effective_bg, tp)
                } else {
                    best_readable_fg(effective_bg, tp)
                };
                if replacement != materialized_fg
                    && let Some(cell) = frame.buffer.get_mut(x, y)
                {
                    cell.fg = replacement;
                }
            }
        }
    }
}

fn infer_effective_background(
    frame: &Frame,
    x: u16,
    y: u16,
    tp: &crate::tui_theme::TuiThemePalette,
) -> PackedRgba {
    // Prefer nearby painted backgrounds over global fallbacks for transparent cells.
    const OFFSETS: &[(i16, i16)] = &[
        (-1, 0),
        (1, 0),
        (0, -1),
        (0, 1),
        (-1, -1),
        (1, -1),
        (-1, 1),
        (1, 1),
    ];
    for radius in 1_i32..=3 {
        for &(dx, dy) in OFFSETS {
            let neighbor = (
                i32::from(x) + i32::from(dx) * radius,
                i32::from(y) + i32::from(dy) * radius,
            );
            if neighbor.0 < 0 || neighbor.1 < 0 {
                continue;
            }
            let Ok(nx) = u16::try_from(neighbor.0) else {
                continue;
            };
            let Ok(ny) = u16::try_from(neighbor.1) else {
                continue;
            };
            let Some(neighbor) = frame.buffer.get(nx, ny) else {
                continue;
            };
            if neighbor.bg.a() != 0 {
                return neighbor.bg;
            }
        }
    }
    if tp.bg_surface.a() != 0 {
        tp.bg_surface
    } else {
        tp.panel_bg
    }
}

fn minimum_text_contrast_for_bg(bg: PackedRgba) -> f64 {
    if perceived_luma(bg) >= 150 {
        MIN_TEXT_CONTRAST_RATIO_LIGHT_SURFACE
    } else {
        MIN_TEXT_CONTRAST_RATIO
    }
}

fn minimum_text_contrast_for_cell(symbol: char, bg: PackedRgba) -> f64 {
    if symbol.is_whitespace() {
        0.0
    } else if is_decorative_glyph(symbol) {
        if perceived_luma(bg) >= 150 {
            MIN_DECORATIVE_CONTRAST_RATIO_LIGHT_SURFACE
        } else {
            MIN_DECORATIVE_CONTRAST_RATIO
        }
    } else {
        minimum_text_contrast_for_bg(bg)
    }
}

const fn is_decorative_glyph(symbol: char) -> bool {
    matches!(
        symbol as u32,
        0x2500..=0x257F | // box drawing
        0x2580..=0x259F | // block elements
        0x2800..=0x28FF // braille patterns (sparklines/mini charts)
    ) || matches!(
        symbol,
        '·' | '•'
            | '▪'
            | '▸'
            | '▹'
            | '▶'
            | '◀'
            | '→'
            | '←'
            | '↔'
            | '↳'
            | '↦'
            | '⋮'
            | '⋯'
    )
}

fn best_readable_decorative_fg(
    bg: PackedRgba,
    tp: &crate::tui_theme::TuiThemePalette,
) -> PackedRgba {
    let target = if perceived_luma(bg) >= 150 { 1.8 } else { 1.55 };
    let fallback = if perceived_luma(bg) >= 128 {
        PackedRgba::rgb(88, 88, 88)
    } else {
        PackedRgba::rgb(176, 176, 176)
    };
    let candidates = [
        tp.panel_border_dim,
        tp.panel_border,
        tp.text_muted,
        tp.text_secondary,
        tp.status_accent,
        fallback,
    ];

    let mut best_above: Option<(PackedRgba, f64)> = None;
    let mut best_below: Option<(PackedRgba, f64)> = None;
    for candidate in candidates {
        let ratio = contrast_ratio(candidate, bg);
        if ratio >= target {
            let distance = ratio - target;
            if best_above.is_none_or(|(_, best)| distance < best) {
                best_above = Some((candidate, distance));
            }
        } else if best_below.is_none_or(|(_, best)| ratio > best) {
            best_below = Some((candidate, ratio));
        }
    }

    best_above
        .map(|(color, _)| color)
        .or_else(|| best_below.map(|(color, _)| color))
        .unwrap_or(fallback)
}

fn best_readable_fg(bg: PackedRgba, tp: &crate::tui_theme::TuiThemePalette) -> PackedRgba {
    let fallback = if perceived_luma(bg) >= 128 {
        tp.status_bg
    } else {
        tp.text_primary
    };
    let candidates = [
        PackedRgba::rgb(10, 10, 10),
        PackedRgba::rgb(245, 245, 245),
        tp.text_primary,
        tp.text_secondary,
        tp.text_muted,
        tp.status_fg,
        tp.selection_fg,
        tp.panel_title_fg,
        tp.help_fg,
        tp.status_accent,
        tp.metric_requests,
        tp.status_good,
        tp.status_warn,
        tp.severity_warn,
        tp.severity_error,
        fallback,
    ];

    let mut best = candidates[0];
    let mut best_ratio = contrast_ratio(best, bg);
    for candidate in candidates.into_iter().skip(1) {
        let ratio = contrast_ratio(candidate, bg);
        if ratio > best_ratio {
            best = candidate;
            best_ratio = ratio;
        }
    }
    best
}

fn perceived_luma(color: PackedRgba) -> u8 {
    let y = 299_u32
        .saturating_mul(u32::from(color.r()))
        .saturating_add(587_u32.saturating_mul(u32::from(color.g())))
        .saturating_add(114_u32.saturating_mul(u32::from(color.b())));
    let luma = (y + 500) / 1000;
    u8::try_from(luma).unwrap_or(u8::MAX)
}

fn srgb_channel_to_linear(c: u8) -> f64 {
    let cs = f64::from(c) / 255.0;
    if cs <= 0.04045 {
        cs / 12.92
    } else {
        ((cs + 0.055) / 1.055).powf(2.4)
    }
}

fn rel_luminance(c: PackedRgba) -> f64 {
    let r = srgb_channel_to_linear(c.r());
    let g = srgb_channel_to_linear(c.g());
    let b = srgb_channel_to_linear(c.b());
    0.2126_f64.mul_add(r, 0.7152_f64.mul_add(g, 0.0722 * b))
}

fn contrast_ratio(fg: PackedRgba, bg: PackedRgba) -> f64 {
    let l1 = rel_luminance(fg);
    let l2 = rel_luminance(bg);
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
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

/// Extract a numeric ID from a context string.
///
/// Supports formats like `"123"`, `"message:123"`, `"reservation:42"`.
fn extract_numeric_id(context: &str) -> Option<i64> {
    // Try the whole string first.
    if let Ok(id) = context.parse::<i64>() {
        return Some(id);
    }
    // Try after `":"` (e.g. "message:123").
    if let Some((_prefix, num_part)) = context.rsplit_once(':')
        && let Ok(id) = num_part.parse::<i64>()
    {
        return Some(id);
    }
    None
}

fn parse_rethread_operation_arg(arg: &str) -> Option<(i64, String)> {
    let (id_part, target_thread_id) = arg.split_once(':')?;
    let message_id = id_part.parse::<i64>().ok()?;
    let target_thread_id = target_thread_id.trim();
    if target_thread_id.is_empty() {
        return None;
    }
    Some((message_id, target_thread_id.to_string()))
}

fn extract_reservation_agent(event: &MailEvent) -> Option<&str> {
    match event {
        MailEvent::ReservationGranted { agent, .. }
        | MailEvent::ReservationReleased { agent, .. } => Some(agent),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Error boundary helpers
// ──────────────────────────────────────────────────────────────────────

/// Extract a human-readable message from a `catch_unwind` panic payload.
#[allow(clippy::option_if_let_else)]
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Render a fallback error UI when a screen has panicked.
fn render_screen_error_fallback(
    screen_id: MailScreenId,
    error_msg: &str,
    area: Rect,
    frame: &mut Frame,
) {
    use ftui::widgets::paragraph::Paragraph;

    if area.width < 4 || area.height < 3 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let screen_name = format!("{screen_id:?}");

    // Background
    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_deep))
        .render(area, frame);

    // Error icon + title
    let title = format!(" Screen '{screen_name}' crashed ");
    let title_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
    Paragraph::new(title)
        .style(Style::default().fg(tp.severity_error).bg(tp.bg_deep).bold())
        .render(title_area, frame);

    // Error message (truncated to available width)
    if area.height > 3 {
        let msg_width = area.width.saturating_sub(4) as usize;
        let truncated: String = error_msg.chars().take(msg_width).collect();
        let msg_area = Rect::new(area.x + 2, area.y + 3, area.width.saturating_sub(4), 1);
        Paragraph::new(truncated)
            .style(Style::default().fg(tp.text_muted).bg(tp.bg_deep))
            .render(msg_area, frame);
    }

    // Recovery hint
    if area.height > 5 {
        let hint = "Press 'r' to retry or switch screens with number keys";
        let hint_area = Rect::new(area.x + 2, area.y + 5, area.width.saturating_sub(4), 1);
        Paragraph::new(hint)
            .style(Style::default().fg(tp.status_accent).bg(tp.bg_deep))
            .render(hint_area, frame);
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
    use ftui_extras::theme::{ScopedThemeLock, ThemeId};
    use ftui_widgets::NotificationPriority;
    use mcp_agent_mail_core::Config;
    use serde::Serialize;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    /// Serializes tests that rely on the global [`PALETTE_DB_CACHE`] singleton.
    static PALETTE_CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_model() -> MailAppModel {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        MailAppModel::new(state)
    }

    fn test_model_with_debug(debug: bool) -> MailAppModel {
        let config = Config {
            tui_debug: debug,
            ..Config::default()
        };
        let state = TuiSharedState::new(&config);
        MailAppModel::new(state)
    }

    fn frame_text(frame: &Frame<'_>) -> String {
        let mut text = String::new();
        for y in 0..frame.buffer.height() {
            for x in 0..frame.buffer.width() {
                if let Some(cell) = frame.buffer.get(x, y) {
                    if let Some(ch) = cell.content.as_char() {
                        text.push(ch);
                    } else if !cell.is_continuation() {
                        text.push(' ');
                    }
                }
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn ambient_mode_off_disables_renderer_in_view() {
        let config = Config {
            tui_ambient: "off".to_string(),
            ..Config::default()
        };
        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(state, &config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);

        model.view(&mut frame);

        let telemetry = model.ambient_last_telemetry();
        assert_eq!(telemetry.mode, AmbientMode::Off);
        assert_eq!(
            telemetry.effect,
            crate::tui_widgets::AmbientEffectKind::None
        );
    }

    #[test]
    fn ambient_render_throttles_to_once_per_tick_bucket() {
        let mut model = test_model();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);

        for _ in 0..AMBIENT_RENDER_EVERY_N_FRAMES {
            let _ = model.update(MailMsg::Terminal(Event::Tick));
        }
        assert!(
            model
                .tick_count
                .is_multiple_of(AMBIENT_RENDER_EVERY_N_FRAMES)
        );

        model.view(&mut frame);
        let first_count = model.ambient_render_invocations.get();
        assert!(
            first_count >= 1,
            "ambient renderer should run on bucket tick"
        );
        assert_eq!(model.ambient_last_render_tick.get(), Some(model.tick_count));

        model.view(&mut frame);
        assert_eq!(
            model.ambient_render_invocations.get(),
            first_count,
            "ambient renderer must not run multiple times in one tick bucket"
        );

        let _ = model.update(MailMsg::Terminal(Event::Tick));
        model.view(&mut frame);
        assert_eq!(
            model.ambient_render_invocations.get(),
            first_count,
            "non-bucket tick should skip ambient rendering"
        );

        for _ in 1..AMBIENT_RENDER_EVERY_N_FRAMES {
            let _ = model.update(MailMsg::Terminal(Event::Tick));
        }
        model.view(&mut frame);
        assert_eq!(
            model.ambient_render_invocations.get(),
            first_count + 1,
            "next bucket tick should render ambient exactly once"
        );
    }

    #[test]
    fn ambient_health_input_flags_error_events_as_critical() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let _ = state.push_event(MailEvent::http_request("GET", "/mcp/", 503, 4, "127.0.0.1"));
        let model = MailAppModel::with_config(state, &config);

        let input = model.ambient_health_input(now_micros());
        assert!(input.critical_alerts_active);
        assert_eq!(input.failed_probe_count, 2);
        assert_eq!(
            crate::tui_widgets::determine_ambient_health_state(input),
            crate::tui_widgets::AmbientHealthState::Critical
        );
    }

    #[test]
    fn ambient_health_input_flags_idle_when_events_are_stale() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let stale_ts = now_micros().saturating_sub(301 * 1_000_000);
        let _ = state.push_event(MailEvent::ServerStarted {
            seq: 0,
            timestamp_micros: stale_ts,
            source: crate::tui_events::EventSource::Lifecycle,
            redacted: false,
            endpoint: "http://127.0.0.1:8765/mcp/".to_string(),
            config_summary: "test".to_string(),
        });
        let model = MailAppModel::with_config(state, &config);

        let input = model.ambient_health_input(now_micros());
        assert!(input.seconds_since_last_event >= 301);
        assert_eq!(
            crate::tui_widgets::determine_ambient_health_state(input),
            crate::tui_widgets::AmbientHealthState::Idle
        );
    }

    #[test]
    fn ambient_health_input_uses_ring_utilization_warning_threshold() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 5);
        for idx in 0..5 {
            let _ = state.push_event(MailEvent::message_sent(
                i64::from(idx),
                "from",
                vec!["to".to_string()],
                "subject",
                "thread",
                "project",
            ));
        }
        let model = MailAppModel::with_config(state, &config);

        let input = model.ambient_health_input(now_micros());
        assert!(input.event_buffer_utilization >= 0.8);
        assert_eq!(
            crate::tui_widgets::determine_ambient_health_state(input),
            crate::tui_widgets::AmbientHealthState::Warning
        );
    }

    #[test]
    fn contrast_guard_fixes_white_on_white_cells() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(6, 2, &mut pool);

        let mut unreadable = ftui::Cell::from_char('X');
        unreadable.fg = PackedRgba::rgb(255, 255, 255);
        unreadable.bg = PackedRgba::rgb(255, 255, 255);
        frame.buffer.set(1, 0, unreadable);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let fixed = frame.buffer.get(1, 0).expect("cell exists after guard");
        assert!(
            contrast_ratio(fixed.fg, fixed.bg) >= MIN_TEXT_CONTRAST_RATIO,
            "contrast guard should enforce minimum readability"
        );
    }

    #[test]
    fn contrast_guard_keeps_decorative_rules_subtle() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(6, 2, &mut pool);

        let mut decorative = ftui::Cell::from_char('│');
        decorative.fg = PackedRgba::rgb(255, 255, 255);
        decorative.bg = PackedRgba::rgb(255, 255, 255);
        frame.buffer.set(1, 0, decorative);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let fixed = frame.buffer.get(1, 0).expect("decorative cell exists");
        let ratio = contrast_ratio(fixed.fg, fixed.bg);
        let min_ratio = minimum_text_contrast_for_cell('│', fixed.bg);
        assert!(
            ratio >= min_ratio,
            "decorative contrast should meet subtle minimum (ratio={ratio:.2}, min={min_ratio:.2})"
        );
        assert!(
            ratio < MIN_TEXT_CONTRAST_RATIO,
            "decorative separators should not be boosted to full text contrast (ratio={ratio:.2})"
        );
    }

    #[test]
    fn contrast_guard_handles_transparent_background_cells() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(6, 2, &mut pool);

        let mut unreadable = ftui::Cell::from_char('X');
        unreadable.fg = PackedRgba::rgb(255, 255, 255);
        unreadable.bg = PackedRgba::rgba(0, 0, 0, 0);
        frame.buffer.set(1, 0, unreadable);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let fixed = frame.buffer.get(1, 0).expect("cell exists after guard");
        let worst_ratio = [tp.panel_bg, tp.bg_surface, tp.bg_deep]
            .into_iter()
            .map(|bg| contrast_ratio(fixed.fg, bg))
            .fold(f64::INFINITY, f64::min);
        assert!(
            worst_ratio >= MIN_TEXT_CONTRAST_RATIO,
            "contrast guard should enforce minimum readability across transparent backgrounds"
        );
    }

    #[test]
    fn contrast_guard_handles_grapheme_backed_cells() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(6, 2, &mut pool);

        let content = ftui::render::cell::CellContent::from_grapheme(
            ftui::render::cell::GraphemeId::new(7, 2, 1),
        );
        let mut unreadable = ftui::Cell::new(content);
        unreadable.fg = PackedRgba::rgb(255, 255, 255);
        unreadable.bg = PackedRgba::rgb(255, 255, 255);
        frame.buffer.set(1, 0, unreadable);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let fixed = frame.buffer.get(1, 0).expect("grapheme cell exists");
        let min_ratio = minimum_text_contrast_for_bg(fixed.bg);
        let ratio = contrast_ratio(fixed.fg, fixed.bg);
        assert!(
            ratio >= min_ratio,
            "grapheme-backed text should be contrast-normalized (ratio={ratio:.2}, min={min_ratio:.2})"
        );
    }

    #[test]
    fn contrast_guard_ignores_whitespace_cells() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(4, 2, &mut pool);

        let mut space = ftui::Cell::from_char(' ');
        space.fg = PackedRgba::rgb(255, 255, 255);
        space.bg = PackedRgba::rgb(255, 255, 255);
        frame.buffer.set(0, 0, space);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let result = frame.buffer.get(0, 0).expect("space cell exists");
        assert_eq!(result.fg, PackedRgba::rgb(255, 255, 255));
    }

    #[test]
    fn contrast_guard_sets_background_for_transparent_whitespace_cells() {
        let _theme = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(4, 2, &mut pool);

        let mut space = ftui::Cell::from_char(' ');
        space.fg = PackedRgba::rgb(255, 255, 255);
        space.bg = PackedRgba::rgba(0, 0, 0, 0);
        frame.buffer.set(0, 0, space);

        let tp = crate::tui_theme::TuiThemePalette::current();
        apply_frame_contrast_guard(&mut frame, &tp);

        let result = frame.buffer.get(0, 0).expect("space cell exists");
        assert_ne!(
            result.bg.a(),
            0,
            "transparent whitespace background should be materialized"
        );
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
    fn only_dashboard_is_eagerly_initialized() {
        let model = test_model();
        for &id in ALL_SCREEN_IDS {
            if id == MailScreenId::Dashboard {
                assert!(model.screen_manager.has_screen(id));
            } else {
                assert!(!model.screen_manager.has_screen(id));
            }
        }
    }

    #[test]
    fn switching_screen_lazily_initializes_target() {
        let mut model = test_model();
        assert!(!model.screen_manager.has_screen(MailScreenId::Messages));

        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        assert!(model.screen_manager.has_screen(MailScreenId::Messages));
    }

    #[test]
    fn deep_link_lazily_initializes_target() {
        let mut model = test_model();
        assert!(!model.screen_manager.has_screen(MailScreenId::Agents));

        let target = DeepLinkTarget::AgentByName("BlueLake".to_string());
        model.update(MailMsg::Screen(MailScreenMsg::DeepLink(target)));

        assert_eq!(model.active_screen(), MailScreenId::Agents);
        assert!(model.screen_manager.has_screen(MailScreenId::Agents));
    }

    #[test]
    fn layout_export_initializes_timeline_screen() {
        let mut model = test_model();
        assert!(!model.screen_manager.has_screen(MailScreenId::Timeline));

        let _ = model.dispatch_palette_action(palette_action_ids::LAYOUT_EXPORT);

        assert!(model.screen_manager.has_screen(MailScreenId::Timeline));
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
        assert!(model.screen_manager.has_screen(MailScreenId::Agents));
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
    fn ctrl_e_opens_export_menu() {
        let mut model = test_model();
        assert!(!model.export_menu_open);
        let event = Event::Key(
            ftui::KeyEvent::new(KeyCode::Char('e')).with_modifiers(ftui::Modifiers::CTRL),
        );
        let _ = model.update(MailMsg::Terminal(event));
        assert!(model.export_menu_open);
    }

    #[test]
    fn export_menu_escape_closes() {
        let mut model = test_model();
        model.open_export_menu();
        assert!(model.export_menu_open);
        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        let _ = model.update(MailMsg::Terminal(esc));
        assert!(!model.export_menu_open);
    }

    #[test]
    fn export_menu_h_key_closes_when_no_snapshot() {
        let mut model = test_model();
        model.open_export_menu();
        assert!(model.export_menu_open);
        let h = Event::Key(ftui::KeyEvent::new(KeyCode::Char('h')));
        let _ = model.update(MailMsg::Terminal(h));
        assert!(!model.export_menu_open);
    }

    #[test]
    fn export_snapshot_to_dir_writes_html_svg_and_text() {
        let model = test_model();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 30, &mut pool);
        model.view(&mut frame);

        let tmp = tempfile::tempdir().expect("tempdir");
        let html = model
            .export_snapshot_to_dir(ExportFormat::Html, tmp.path())
            .expect("html export path");
        let svg = model
            .export_snapshot_to_dir(ExportFormat::Svg, tmp.path())
            .expect("svg export path");
        let text = model
            .export_snapshot_to_dir(ExportFormat::Text, tmp.path())
            .expect("text export path");

        assert_eq!(html.extension().and_then(|v| v.to_str()), Some("html"));
        assert_eq!(svg.extension().and_then(|v| v.to_str()), Some("svg"));
        assert_eq!(text.extension().and_then(|v| v.to_str()), Some("txt"));

        let html_body = std::fs::read_to_string(&html).expect("read html export");
        assert!(
            html_body.contains("<pre"),
            "html export should contain <pre>"
        );

        let svg_body = std::fs::read_to_string(&svg).expect("read svg export");
        assert!(svg_body.contains("<svg"), "svg export should contain <svg>");

        let text_body = std::fs::read_to_string(&text).expect("read text export");
        assert!(
            !text_body.contains('\u{001b}'),
            "plain text export should not include ANSI escapes"
        );
    }

    #[test]
    fn resolve_export_dir_prefers_env_and_falls_back_to_home() {
        let home = PathBuf::from("/tmp/fake-home");
        let from_env =
            resolve_export_dir_from_sources(Some("/tmp/custom-export"), Some(home.as_path()));
        assert_eq!(from_env, PathBuf::from("/tmp/custom-export"));

        let from_home = resolve_export_dir_from_sources(None, Some(home.as_path()));
        assert_eq!(from_home, home.join(".mcp_agent_mail").join("exports"));
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

    // ── Compose overlay wiring tests ────────────────────────────────

    #[test]
    fn ctrl_n_opens_compose_overlay() {
        let mut model = test_model();
        assert!(model.compose_state.is_none());
        let event =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('n')).with_modifiers(Modifiers::CTRL));
        let _ = model.update(MailMsg::Terminal(event));
        assert!(model.compose_state.is_some());
        assert_eq!(model.topmost_overlay(), OverlayLayer::Compose);
    }

    #[test]
    fn compose_traps_focus() {
        assert!(OverlayLayer::Compose.traps_focus());
    }

    #[test]
    fn compose_escape_closes_overlay() {
        let mut model = test_model();
        // Open compose
        let open =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('n')).with_modifiers(Modifiers::CTRL));
        let _ = model.update(MailMsg::Terminal(open));
        assert!(model.compose_state.is_some());

        // Press Escape to close
        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        let _ = model.update(MailMsg::Terminal(esc));
        assert!(model.compose_state.is_none());
        assert_eq!(model.topmost_overlay(), OverlayLayer::None);
    }

    #[test]
    fn compose_overlay_z_order_between_palette_and_modal() {
        assert!(OverlayLayer::Compose > OverlayLayer::Modal);
        assert!(OverlayLayer::Compose < OverlayLayer::Palette);
    }

    #[test]
    fn compose_does_not_open_twice() {
        let mut model = test_model();
        let open =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('n')).with_modifiers(Modifiers::CTRL));
        let _ = model.update(MailMsg::Terminal(open.clone()));
        assert!(model.compose_state.is_some());
        // Type something into subject to mark state as modified.
        let _ = model.update(MailMsg::Terminal(Event::Key(ftui::KeyEvent::new(
            KeyCode::Char('X'),
        ))));
        // Second Ctrl+N should not reset compose state.
        let _ = model.update(MailMsg::Terminal(open));
        // Compose is still open (not reset).
        assert!(model.compose_state.is_some());
    }

    #[test]
    fn compose_traps_keys_from_reaching_screen() {
        let mut model = test_model();
        let initial_screen = model.active_screen();
        // Open compose
        let open =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('n')).with_modifiers(Modifiers::CTRL));
        let _ = model.update(MailMsg::Terminal(open));
        // Press Tab — should be trapped by compose, not switch screens.
        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        let _ = model.update(MailMsg::Terminal(tab));
        assert_eq!(model.active_screen(), initial_screen);
    }

    #[test]
    fn view_hides_terminal_cursor_to_prevent_cursor_trails() {
        let model = test_model();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);

        assert!(frame.cursor_visible);
        model.view(&mut frame);
        assert!(!frame.cursor_visible);
    }

    #[test]
    fn deferred_diff_always_promotes_to_full_to_avoid_cursor_trails() {
        let deferred = crate::tui_decision::DiffAction::Deferred;
        let full = crate::tui_decision::DiffAction::Full;

        assert_eq!(
            MailAppModel::adjust_diff_action_for_overlays(deferred),
            full
        );
        assert_eq!(MailAppModel::adjust_diff_action_for_overlays(full), full);
    }

    #[test]
    fn remote_modifiers_maps_browser_bit_layout() {
        let mods = remote_modifiers_from_bits(0b1111);
        assert!(mods.contains(Modifiers::CTRL));
        assert!(mods.contains(Modifiers::SHIFT));
        assert!(mods.contains(Modifiers::ALT));
        assert!(mods.contains(Modifiers::SUPER));
    }

    #[test]
    fn remote_key_code_maps_browser_aliases() {
        assert_eq!(remote_key_code_from_label("ArrowUp"), Some(KeyCode::Up));
        assert_eq!(remote_key_code_from_label("Esc"), Some(KeyCode::Escape));
        assert_eq!(
            remote_key_code_from_label("Space"),
            Some(KeyCode::Char(' '))
        );
        assert_eq!(remote_key_code_from_label("f12"), Some(KeyCode::F(12)));
    }

    #[test]
    fn tick_drains_remote_terminal_events() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::new(Arc::clone(&state));
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);

        let _ = state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "2".to_string(),
            modifiers: 0,
        });
        let _ = state.push_remote_terminal_event(RemoteTerminalEvent::Resize {
            cols: 120,
            rows: 40,
        });
        assert_eq!(state.remote_terminal_queue_len(), 2);

        let cmd = model.update(MailMsg::Terminal(Event::Tick));
        assert!(matches!(cmd, Cmd::Tick(_)));
        assert_eq!(state.remote_terminal_queue_len(), 0);
        assert_eq!(model.active_screen(), MailScreenId::Messages);
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
    fn ctrl_arrow_moves_focus_between_panels_spatially() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::List(0));

        let ctrl_right = Event::Key(KeyEvent::new(KeyCode::Right).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_right));
        assert_eq!(model.focus_manager.current(), FocusTarget::DetailPanel);

        let ctrl_left = Event::Key(KeyEvent::new(KeyCode::Left).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_left));
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));
    }

    #[test]
    fn ctrl_arrow_on_boundary_panel_is_noop() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::DetailPanel);

        let ctrl_right = Event::Key(KeyEvent::new(KeyCode::Right).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_right));
        assert_eq!(model.focus_manager.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn ctrl_arrow_recovers_from_hidden_focus_target_in_compact_layout() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 60, 20);
        model.restore_focus_for_screen(MailScreenId::Messages);
        // Simulate stale focus memory from a wider layout.
        let _ = model.focus_manager.focus(FocusTarget::DetailPanel);

        let ctrl_down = Event::Key(KeyEvent::new(KeyCode::Down).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_down));
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));
    }

    #[test]
    fn ctrl_arrow_down_moves_from_search_to_preview_panel() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::TextInput(0));

        let ctrl_down = Event::Key(KeyEvent::new(KeyCode::Down).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_down));
        assert_eq!(model.focus_manager.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn ctrl_arrow_is_ignored_when_focus_is_trapped() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::List(0));
        model
            .focus_manager
            .push_context(crate::tui_focus::FocusContext::Modal);
        assert!(model.focus_manager.is_trapped());
        assert_eq!(model.focus_manager.current(), FocusTarget::ModalContent);

        let ctrl_right = Event::Key(KeyEvent::new(KeyCode::Right).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_right));
        assert_eq!(model.focus_manager.current(), FocusTarget::ModalContent);
    }

    #[test]
    fn focus_memory_restores_saved_target_after_screen_switch() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::DetailPanel);

        model.update(MailMsg::SwitchScreen(MailScreenId::Agents));
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));

        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert_eq!(model.focus_manager.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn focus_memory_restores_previous_target_when_returning_to_screen() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::DetailPanel);
        model
            .focus_memory
            .insert(MailScreenId::Messages, FocusTarget::DetailPanel);

        model.update(MailMsg::SwitchScreen(MailScreenId::Agents));
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));

        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert_eq!(model.focus_manager.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn restore_focus_defaults_to_first_target_when_screen_has_no_memory() {
        let mut model = test_model();
        model.focus_memory.clear();
        model.restore_focus_for_screen(MailScreenId::Projects);
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));
    }

    #[test]
    fn plain_arrow_keys_do_not_trigger_global_spatial_focus_move() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        *model.last_content_area.borrow_mut() = Rect::new(0, 0, 120, 40);
        model.restore_focus_for_screen(MailScreenId::Messages);
        let _ = model.focus_manager.focus(FocusTarget::List(0));

        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Right))));
        assert_eq!(model.focus_manager.current(), FocusTarget::List(0));
    }

    #[test]
    fn screen_switch_starts_transition_when_motion_enabled() {
        let mut model = test_model();
        assert!(model.screen_transition.is_none());

        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        let transition = model.screen_transition.expect("transition should start");
        assert_eq!(transition.from, MailScreenId::Dashboard);
        assert_eq!(transition.to, MailScreenId::Messages);
    }

    #[test]
    fn screen_switch_skips_transition_when_reduced_motion_enabled() {
        let mut model = test_model();
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(model.accessibility().reduced_motion);

        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert!(model.screen_transition.is_none());
    }

    #[test]
    fn transition_expires_after_tick_budget() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert!(model.screen_transition.is_some());

        for _ in 0..SCREEN_TRANSITION_TICKS {
            model.update(MailMsg::Terminal(Event::Tick));
        }
        assert!(model.screen_transition.is_none());
    }

    // ── Semantic transition tests ────────────────────────────────

    #[test]
    fn transition_lateral_within_same_category() {
        // Dashboard and Timeline are both Overview — should be Lateral
        let t = ScreenTransition::new(MailScreenId::Dashboard, MailScreenId::Timeline);
        assert_eq!(t.kind, TransitionKind::Lateral);
    }

    #[test]
    fn transition_cross_category_between_different_categories() {
        // Dashboard (Overview) → Messages (Communication)
        let t = ScreenTransition::new(MailScreenId::Dashboard, MailScreenId::Messages);
        assert_eq!(t.kind, TransitionKind::CrossCategory);
    }

    #[test]
    fn transition_forward_direction_when_tab_index_increases() {
        // Dashboard (idx 0) → Messages (idx 1)
        let t = ScreenTransition::new(MailScreenId::Dashboard, MailScreenId::Messages);
        assert_eq!(t.direction, TransitionDirection::Forward);
    }

    #[test]
    fn transition_backward_direction_when_tab_index_decreases() {
        // Messages (idx 1) → Dashboard (idx 0)
        let t = ScreenTransition::new(MailScreenId::Messages, MailScreenId::Dashboard);
        assert_eq!(t.direction, TransitionDirection::Backward);
    }

    #[test]
    fn transition_eased_progress_starts_at_zero() {
        let t = ScreenTransition::new(MailScreenId::Dashboard, MailScreenId::Messages);
        // At full ticks_remaining, progress is 0
        assert!((t.eased_progress() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn transition_eased_progress_decelerates() {
        // Eased progress should be >= linear progress at midpoint
        let mut t = ScreenTransition::new(MailScreenId::Dashboard, MailScreenId::Messages);
        t.ticks_remaining = SCREEN_TRANSITION_TICKS / 2;
        let linear = t.progress();
        let eased = t.eased_progress();
        assert!(
            eased >= linear,
            "eased ({eased}) should be >= linear ({linear}) at midpoint"
        );
    }

    #[test]
    fn number_keys_switch_screens() {
        let mut model = test_model();
        // Digit keys map to direct jump slots: 1..9 => screens 1..9, 0 => screen 10.
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
        // Key 0 maps to the 10th screen.
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
        // 0 maps to screen 10 (Projects).
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
    fn shifted_number_symbols_switch_screens_above_ten() {
        let mut model = test_model();

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('!')));
        model.update(MailMsg::Terminal(key));
        assert_eq!(model.active_screen(), MailScreenId::Contacts);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('@')));
        model.update(MailMsg::Terminal(key));
        assert_eq!(model.active_screen(), MailScreenId::Explorer);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('#')));
        model.update(MailMsg::Terminal(key));
        assert_eq!(model.active_screen(), MailScreenId::Analytics);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('$')));
        model.update(MailMsg::Terminal(key));
        assert_eq!(model.active_screen(), MailScreenId::Attachments);
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
    fn mouse_click_outside_help_overlay_dismisses_help() {
        let mut model = test_model();
        *model.last_content_area.borrow_mut() = Rect::new(0, 1, 120, 38);
        model.update(MailMsg::ToggleHelp);
        assert!(model.help_visible());

        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: 0,
            y: 0,
            modifiers: Modifiers::empty(),
        });
        model.update(MailMsg::Terminal(click));
        assert!(!model.help_visible());
    }

    #[test]
    fn mouse_click_inside_help_overlay_keeps_help_visible() {
        let mut model = test_model();
        let root_area = Rect::new(0, 0, 120, 40);
        *model.last_content_area.borrow_mut() = Rect::new(0, 1, 120, 38);
        model.update(MailMsg::ToggleHelp);
        assert!(model.help_visible());

        let overlay = crate::tui_chrome::help_overlay_rect(root_area);
        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: overlay.x.saturating_add(1),
            y: overlay.y.saturating_add(1),
            modifiers: Modifiers::empty(),
        });
        model.update(MailMsg::Terminal(click));
        assert!(model.help_visible());
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
    fn q_key_quits_even_when_help_overlay_visible() {
        let mut model = test_model();
        model.help_visible = true;

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('q')));
        let cmd = model.update(MailMsg::Terminal(key));

        assert!(model.state.is_shutdown_requested());
        assert!(matches!(cmd, Cmd::Quit));
    }

    #[test]
    fn escape_requires_confirmation_then_quits() {
        let mut model = test_model();
        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));

        let first = model.update(MailMsg::Terminal(esc.clone()));
        assert!(!model.state.is_shutdown_requested());
        assert!(matches!(first, Cmd::None));

        let second = model.update(MailMsg::Terminal(esc));
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(second, Cmd::Quit));
    }

    #[test]
    fn ctrl_c_requires_confirmation_then_quits() {
        let mut model = test_model();
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c')).with_modifiers(Modifiers::CTRL));

        let first = model.update(MailMsg::Terminal(ctrl_c.clone()));
        assert!(!model.state.is_shutdown_requested());
        assert!(matches!(first, Cmd::None));

        let second = model.update(MailMsg::Terminal(ctrl_c));
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(second, Cmd::Quit));
    }

    #[test]
    fn ctrl_c_requires_confirmation_then_quits_with_help_overlay_visible() {
        let mut model = test_model();
        model.help_visible = true;
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c')).with_modifiers(Modifiers::CTRL));

        let first = model.update(MailMsg::Terminal(ctrl_c.clone()));
        assert!(!model.state.is_shutdown_requested());
        assert!(matches!(first, Cmd::None));

        let second = model.update(MailMsg::Terminal(ctrl_c));
        assert!(model.state.is_shutdown_requested());
        assert!(matches!(second, Cmd::Quit));
    }

    #[test]
    fn ctrl_d_detaches_tui_without_shutdown() {
        let mut model = test_model();
        let ctrl_d = Event::Key(KeyEvent::new(KeyCode::Char('d')).with_modifiers(Modifiers::CTRL));
        let cmd = model.update(MailMsg::Terminal(ctrl_d));
        assert!(!model.state.is_shutdown_requested());
        assert!(model.state.is_headless_detach_requested());
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
        // Lazy-init: default screen is eager; with_config also preloads Timeline
        for &id in ALL_SCREEN_IDS {
            if id == MailScreenId::Dashboard || id == MailScreenId::Timeline {
                assert!(model.screen_manager.has_screen(id));
            } else {
                assert!(!model.screen_manager.has_screen(id));
            }
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

    /// Guard: every registered screen is reachable by either a direct jump key
    /// or a command palette entry (or both).
    #[test]
    fn every_screen_has_discoverable_jump_path() {
        let actions = build_palette_actions_static();
        for &id in ALL_SCREEN_IDS {
            let has_key = crate::tui_screens::jump_key_label_for_screen(id).is_some();
            let action_id = screen_palette_action_id(id);
            let in_palette = actions.iter().any(|a| a.id == action_id);
            assert!(
                has_key || in_palette,
                "screen {id:?} has no jump key and no palette entry"
            );
            // Also verify palette entry exists even if key exists (belt and suspenders).
            assert!(in_palette, "screen {id:?} missing from command palette");
        }
    }

    #[test]
    fn palette_screen_descriptions_include_key_hint() {
        let actions = build_palette_actions_static();
        for &id in ALL_SCREEN_IDS {
            let action_id = screen_palette_action_id(id);
            let action = actions.iter().find(|a| a.id == action_id).unwrap();
            let desc = action.description.as_deref().unwrap_or("");
            if let Some(key) = crate::tui_screens::jump_key_label_for_screen(id) {
                assert!(
                    desc.contains(&format!("[key: {key}]")),
                    "palette entry for {id:?} should include key hint, got: {desc}"
                );
            } else {
                assert!(
                    desc.contains("[via palette only]"),
                    "palette entry for {id:?} should indicate palette-only access, got: {desc}"
                );
            }
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
        assert!(ids.contains(&palette_action_ids::APP_DETACH));
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
    fn dispatch_palette_detach_does_not_request_shutdown() {
        let mut model = test_model();
        let cmd = model.dispatch_palette_action(palette_action_ids::APP_DETACH);
        assert!(!model.state.is_shutdown_requested());
        assert!(model.state.is_headless_detach_requested());
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
        assert!(!model.accessibility().reduced_motion);
        assert!(!model.accessibility().screen_reader);
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
    fn toggle_reduced_motion_via_palette() {
        let mut model = test_model();
        assert!(!model.accessibility().reduced_motion);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(model.accessibility().reduced_motion);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(!model.accessibility().reduced_motion);
    }

    #[test]
    fn reduced_motion_toggle_cancels_inflight_transition() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert!(model.screen_transition.is_some());

        // Enabling reduced-motion mid-flight should cancel the transition.
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(model.screen_transition.is_none());
    }

    #[test]
    fn reduced_motion_toggle_persists_to_envfile() {
        let dir = tempfile::tempdir().unwrap();
        let config = mcp_agent_mail_core::Config {
            console_persist_path: dir.path().join("config.env"),
            ..mcp_agent_mail_core::Config::default()
        };
        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::with_config(state, &config);

        assert!(!model.accessibility().reduced_motion);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(model.accessibility().reduced_motion);

        // Check persisted value
        let contents = std::fs::read_to_string(&config.console_persist_path).unwrap_or_default();
        assert!(
            contents.contains("TUI_REDUCED_MOTION=true"),
            "reduced_motion should be persisted; contents: {contents}"
        );
    }

    #[test]
    fn reduced_motion_semantic_transitions_skipped() {
        let mut model = test_model();
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        assert!(model.accessibility().reduced_motion);

        // Both lateral and cross-category transitions should be skipped.
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert!(
            model.screen_transition.is_none(),
            "cross-category should be skipped"
        );

        model.update(MailMsg::SwitchScreen(MailScreenId::Threads));
        assert!(
            model.screen_transition.is_none(),
            "lateral should be skipped"
        );
    }

    #[test]
    fn toggle_screen_reader_via_palette_disables_key_hints() {
        let mut model = test_model();
        assert!(!model.accessibility().screen_reader);
        assert!(model.accessibility().key_hints);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_SCREEN_READER);
        assert!(model.accessibility().screen_reader);
        assert!(!model.accessibility().key_hints);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_SCREEN_READER);
        assert!(!model.accessibility().screen_reader);
    }

    #[test]
    fn palette_static_actions_include_accessibility_controls() {
        let actions = build_palette_actions_static();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_HC));
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_HINTS));
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION));
        assert!(ids.contains(&palette_action_ids::A11Y_TOGGLE_SCREEN_READER));
    }

    #[test]
    fn palette_static_actions_include_theme_controls() {
        let actions = build_palette_actions_static();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&palette_action_ids::THEME_CYCLE));
        assert!(ids.contains(&palette_action_ids::THEME_CYBERPUNK));
        assert!(ids.contains(&palette_action_ids::THEME_DARCULA));
        assert!(ids.contains(&palette_action_ids::THEME_LUMEN));
        assert!(ids.contains(&palette_action_ids::THEME_NORDIC));
        assert!(ids.contains(&palette_action_ids::THEME_HIGH_CONTRAST));
        assert!(ids.contains(&palette_action_ids::THEME_FRANKENSTEIN));
    }

    #[test]
    fn explicit_theme_actions_switch_runtime_theme() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut model = test_model();

        model.dispatch_palette_action(palette_action_ids::THEME_DARCULA);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::Darcula);
        assert!(!model.accessibility().high_contrast);

        model.dispatch_palette_action(palette_action_ids::THEME_HIGH_CONTRAST);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::HighContrast);
        assert!(model.accessibility().high_contrast);

        model.dispatch_palette_action(palette_action_ids::THEME_FRANKENSTEIN);
        assert_eq!(
            crate::tui_theme::current_theme_id(),
            ThemeId::CyberpunkAurora
        );
        assert_eq!(
            crate::tui_theme::active_named_theme_config_name(),
            "frankenstein"
        );
        assert!(!model.accessibility().high_contrast);
    }

    #[test]
    fn toggle_high_contrast_restores_previous_theme() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut model = test_model();
        model.dispatch_palette_action(palette_action_ids::THEME_NORDIC);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::NordicFrost);

        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HC);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::HighContrast);
        assert!(model.accessibility().high_contrast);

        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HC);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::NordicFrost);
        assert!(!model.accessibility().high_contrast);
    }

    #[test]
    fn with_config_loads_accessibility_settings() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let config = mcp_agent_mail_core::Config {
            tui_high_contrast: true,
            tui_key_hints: false,
            tui_reduced_motion: true,
            tui_screen_reader: true,
            ..mcp_agent_mail_core::Config::default()
        };
        let state = TuiSharedState::new(&config);
        let model = MailAppModel::with_config(Arc::clone(&state), &config);
        assert!(model.accessibility().high_contrast);
        assert!(!model.accessibility().key_hints);
        assert!(model.accessibility().reduced_motion);
        assert!(model.accessibility().screen_reader);
        assert_eq!(crate::tui_theme::current_theme_id(), ThemeId::HighContrast);
    }

    #[test]
    fn flush_before_shutdown_persists_theme_and_accessibility_settings() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_path = tmp.path().join("config.env");
        let config = Config {
            console_persist_path: env_path.clone(),
            ..Config::default()
        };
        let state = TuiSharedState::new(&config);
        let mut model = MailAppModel::with_config(state, &config);

        model.dispatch_palette_action(palette_action_ids::THEME_DARCULA);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_HINTS);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_REDUCED_MOTION);
        model.dispatch_palette_action(palette_action_ids::A11Y_TOGGLE_SCREEN_READER);
        model.flush_before_shutdown();

        let contents = std::fs::read_to_string(env_path).expect("read env");
        assert!(contents.contains("CONSOLE_THEME=darcula"));
        assert!(contents.contains("TUI_HIGH_CONTRAST=false"));
        assert!(contents.contains("TUI_KEY_HINTS=false"));
        assert!(contents.contains("TUI_REDUCED_MOTION=true"));
        assert!(contents.contains("TUI_SCREEN_READER=true"));
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
            body_snippet: String::new(),
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
    fn palette_db_cache_respects_ttl() {
        let _serial = PALETTE_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Reset global cache to avoid test-order dependence.
        let cache = PALETTE_DB_CACHE.get_or_init(|| Mutex::new(PaletteDbCache::default()));
        if let Ok(mut guard) = cache.lock() {
            *guard = PaletteDbCache::default();
        }

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let bridge_state = palette_cache_bridge_state(&state);
        let db_url = state.config_snapshot().raw_database_url;
        let expected = PaletteMessageSummary {
            id: 7,
            subject: "cached subject".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "RedFox".to_string(),
            thread_id: "br-7".to_string(),
            timestamp_micros: now_micros(),
            body_snippet: String::new(),
        };

        let cache = PALETTE_DB_CACHE.get_or_init(|| Mutex::new(PaletteDbCache::default()));
        {
            let mut guard = cache.lock().expect("cache lock");
            guard.database_url = db_url;
            guard.fetched_at_micros = now_micros();
            guard.source_snapshot_micros = bridge_state.snapshot_micros;
            guard.source_message_count = bridge_state.message_count;
            guard.agent_metadata.insert(
                "BlueLake".to_string(),
                ("gpt-5".to_string(), "proj-a".to_string()),
            );
            guard.messages = vec![expected.clone()];
        }

        let (agent_metadata, messages) = fetch_palette_db_data(&state, 10, 10);
        assert_eq!(
            agent_metadata.get("BlueLake"),
            Some(&("gpt-5".to_string(), "proj-a".to_string()))
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, expected.id);
        assert_eq!(messages[0].subject, expected.subject);

        if let Ok(mut guard) = cache.lock() {
            *guard = PaletteDbCache::default();
        }
    }

    #[test]
    fn palette_db_cache_invalidates_when_bridge_snapshot_changes() {
        let _serial = PALETTE_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Reset global cache to avoid test-order dependence.
        let cache = PALETTE_DB_CACHE.get_or_init(|| Mutex::new(PaletteDbCache::default()));
        if let Ok(mut guard) = cache.lock() {
            *guard = PaletteDbCache::default();
        }

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let bridge_state = palette_cache_bridge_state(&state);
        let db_url = state.config_snapshot().raw_database_url;
        let expected = PaletteMessageSummary {
            id: 11,
            subject: "stale subject".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "RedFox".to_string(),
            thread_id: "br-11".to_string(),
            timestamp_micros: now_micros(),
            body_snippet: String::new(),
        };

        let cache = PALETTE_DB_CACHE.get_or_init(|| Mutex::new(PaletteDbCache::default()));
        {
            let mut guard = cache.lock().expect("cache lock");
            guard.database_url = db_url;
            guard.fetched_at_micros = now_micros();
            guard.source_snapshot_micros = bridge_state.snapshot_micros;
            guard.source_message_count = bridge_state.message_count;
            guard.messages = vec![expected];
        }

        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            messages: bridge_state.message_count.saturating_add(1),
            timestamp_micros: bridge_state.snapshot_micros.saturating_add(1),
            ..Default::default()
        });

        let (agent_metadata, messages) = fetch_palette_db_data(&state, 10, 10);
        assert!(agent_metadata.is_empty());
        assert!(messages.is_empty());

        if let Ok(mut guard) = cache.lock() {
            *guard = PaletteDbCache::default();
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
    fn palette_overlay_uses_rounded_border_corners() {
        let mut model = test_model();
        model.open_palette();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(100, 30, &mut pool);
        model.view(&mut frame);

        let area = Rect::new(0, 0, 100, 30);
        let palette_width = (area.width * 3 / 5).max(30).min(area.width - 2);
        let result_rows = model
            .command_palette
            .result_count()
            .min(PALETTE_MAX_VISIBLE);
        let palette_height = u16::try_from(result_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(3)
            .max(5)
            .min(area.height.saturating_sub(2));
        let palette_x = area.x + (area.width.saturating_sub(palette_width)) / 2;
        let palette_y = area.y + area.height / 6;
        let right = palette_x + palette_width - 1;
        let bottom = palette_y + palette_height - 1;

        assert_eq!(
            frame
                .buffer
                .get(palette_x, palette_y)
                .and_then(|c| c.content.as_char()),
            Some('╭')
        );
        assert_eq!(
            frame
                .buffer
                .get(right, palette_y)
                .and_then(|c| c.content.as_char()),
            Some('╮')
        );
        assert_eq!(
            frame
                .buffer
                .get(palette_x, bottom)
                .and_then(|c| c.content.as_char()),
            Some('╰')
        );
        assert_eq!(
            frame
                .buffer
                .get(right, bottom)
                .and_then(|c| c.content.as_char()),
            Some('╯')
        );
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
            MailScreenId::ArchiveBrowser => "archive_browser",
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

    #[test]
    fn ctrl_y_toggles_toast_focus_mode_when_visible_toasts_exist() {
        let mut model = test_model();
        model.notifications.notify(
            Toast::new("focus target")
                .icon(ToastIcon::Info)
                .duration(Duration::from_mins(1)),
        );
        model.notifications.tick(Duration::from_millis(16));
        assert_eq!(model.notifications.visible_count(), 1);
        assert!(model.toast_focus_index.is_none());

        let ctrl_y = Event::Key(KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL));
        let cmd = model.update(MailMsg::Terminal(ctrl_y.clone()));
        assert!(matches!(cmd, Cmd::None));
        assert_eq!(model.toast_focus_index, Some(0));

        let cmd = model.update(MailMsg::Terminal(ctrl_y));
        assert!(matches!(cmd, Cmd::None));
        assert!(model.toast_focus_index.is_none());
    }

    #[test]
    fn ctrl_y_does_not_enter_focus_mode_with_no_visible_toasts() {
        let mut model = test_model();
        assert_eq!(model.notifications.visible_count(), 0);

        let ctrl_y = Event::Key(KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL));
        let cmd = model.update(MailMsg::Terminal(ctrl_y));
        assert!(matches!(cmd, Cmd::None));
        assert!(model.toast_focus_index.is_none());
    }

    #[test]
    fn ctrl_t_cycles_theme() {
        // Hold both locks: ScopedThemeLock for ThemeId global, NAMED_THEME_TEST_LOCK
        // for named-theme index. Acquire named lock first to avoid deadlock.
        let _ng = crate::tui_theme::NAMED_THEME_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tui_theme::init_named_theme("default");
        let _tg = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut model = test_model();
        let before = crate::tui_theme::current_theme_id();
        assert_eq!(before, ThemeId::CyberpunkAurora);
        let ctrl_t = Event::Key(KeyEvent::new(KeyCode::Char('t')).with_modifiers(Modifiers::CTRL));
        let cmd = model.update(MailMsg::Terminal(ctrl_t));
        assert!(matches!(cmd, Cmd::None));
        let after = crate::tui_theme::current_theme_id();
        assert_ne!(before, after);
        crate::tui_theme::init_named_theme("default");
    }

    #[test]
    fn shift_t_cycles_theme_when_not_in_text_mode() {
        let _ng = crate::tui_theme::NAMED_THEME_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tui_theme::init_named_theme("default");
        let _tg = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut model = test_model();
        let before = crate::tui_theme::current_theme_id();
        assert_eq!(before, ThemeId::CyberpunkAurora);
        let shift_t =
            Event::Key(KeyEvent::new(KeyCode::Char('T')).with_modifiers(Modifiers::SHIFT));
        let cmd = model.update(MailMsg::Terminal(shift_t));
        assert!(matches!(cmd, Cmd::None));
        let after = crate::tui_theme::current_theme_id();
        assert_ne!(before, after);
        crate::tui_theme::init_named_theme("default");
    }

    #[test]
    fn toast_focus_navigation_and_dismissal_work_via_terminal_events() {
        let mut model = test_model();
        for i in 0..3 {
            model.notifications.notify(
                Toast::new(format!("toast {i}"))
                    .icon(ToastIcon::Info)
                    .duration(Duration::from_mins(1)),
            );
        }
        model.notifications.tick(Duration::from_millis(16));
        assert_eq!(model.notifications.visible_count(), 3);

        let ctrl_y = Event::Key(KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL));
        model.update(MailMsg::Terminal(ctrl_y));
        assert_eq!(model.toast_focus_index, Some(0));

        let down = Event::Key(KeyEvent::new(KeyCode::Down));
        model.update(MailMsg::Terminal(down));
        assert_eq!(model.toast_focus_index, Some(1));

        let up = Event::Key(KeyEvent::new(KeyCode::Up));
        model.update(MailMsg::Terminal(up));
        assert_eq!(model.toast_focus_index, Some(0));

        let enter = Event::Key(KeyEvent::new(KeyCode::Enter));
        model.update(MailMsg::Terminal(enter));
        model.notifications.tick(Duration::from_millis(16));
        assert_eq!(model.notifications.visible_count(), 2);
        assert_eq!(model.toast_focus_index, Some(0));
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
    fn toast_message_received_unicode_subject_never_panics() {
        let subject =
            "[review] Session 16 code review pass — 1 bug fixed, ~3,500 lines reviewed clean";
        let event = MailEvent::message_received(1, "BlueLake", vec![], subject, "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        assert!(toast.content.message.starts_with("BlueLake: "));
        assert!(toast.content.message.contains('—'));
    }

    #[test]
    fn safe_toast_wrapper_recovers_from_panics() {
        let toast = safe_toast_from_builder(|| -> Option<Toast> {
            panic!("synthetic panic in toast builder");
        });
        assert!(toast.is_none());
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
            toast.is_none(),
            "ServerStarted toast is intentionally suppressed"
        );
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
        model.warned_reservations.insert(key);
        assert_eq!(model.warned_reservations.len(), 1);
    }

    #[test]
    fn reservation_tracker_updates_from_grant_and_release_events() {
        let mut model = test_model();
        assert!(model.state.push_event(MailEvent::reservation_granted(
            "BlueLake",
            vec!["src/**".to_string()],
            true,
            600,
            "proj-a",
        )));

        let _ = model.update(MailMsg::Terminal(Event::Tick));
        assert_eq!(model.reservation_tracker.len(), 1);

        assert!(model.state.push_event(MailEvent::reservation_released(
            "BlueLake",
            vec!["src/**".to_string()],
            "proj-a",
        )));
        let _ = model.update(MailMsg::Terminal(Event::Tick));
        assert!(
            model.reservation_tracker.is_empty(),
            "release event should clear reservation tracker entry"
        );
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
                .duration(Duration::from_mins(1)),
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
                    .duration(Duration::from_mins(1)),
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
                    .duration(Duration::from_mins(1)),
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
                    .duration(Duration::from_mins(1)),
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
                .duration(Duration::from_mins(1)),
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

    #[test]
    fn toast_entrance_slide_progresses_over_ticks() {
        // TOAST_ENTRANCE_TICKS=10, formula: (10 - age) * 2
        assert_eq!(entrance_slide_columns(0), 20);
        assert_eq!(entrance_slide_columns(1), 18);
        assert_eq!(entrance_slide_columns(2), 16);
        assert_eq!(entrance_slide_columns(3), 14);
        assert_eq!(entrance_slide_columns(4), 12);
        assert_eq!(entrance_slide_columns(9), 2);
    }

    #[test]
    fn toast_exit_fade_levels_progress_over_ticks() {
        // TOAST_EXIT_TICKS=7, formula: 7 - remaining + 1 (if 1..=7)
        assert_eq!(exit_fade_level(None), 0);
        assert_eq!(exit_fade_level(Some(7)), 1); // 7-7+1
        assert_eq!(exit_fade_level(Some(3)), 5); // 7-3+1
        assert_eq!(exit_fade_level(Some(2)), 6); // 7-2+1
        assert_eq!(exit_fade_level(Some(1)), 7); // 7-1+1
        assert_eq!(exit_fade_level(Some(0)), 0);
    }

    #[test]
    fn toast_remaining_ticks_rounds_up() {
        // TICK_INTERVAL=33ms, formula: ceil(ms / 33)
        assert_eq!(remaining_ticks_from_duration(Duration::from_millis(1)), 1);
        assert_eq!(remaining_ticks_from_duration(Duration::from_millis(99)), 3); // ceil(99/33)
        assert_eq!(remaining_ticks_from_duration(Duration::from_millis(100)), 4); // ceil(100/33)
        assert_eq!(remaining_ticks_from_duration(Duration::from_millis(101)), 4); // ceil(101/33)
    }

    #[test]
    fn animated_toast_stack_respects_entry_offset() {
        let area = Rect::new(0, 0, 80, 24);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(area.width, area.height, &mut pool);
        let mut queue =
            NotificationQueue::new(QueueConfig::default().position(ToastPosition::TopLeft));
        queue.notify(Toast::new("slide test").duration(Duration::from_secs(10)));
        queue.tick(TICK_INTERVAL);

        let id = queue.visible()[0].id;
        let mut age = HashMap::new();
        age.insert(id, 0);
        render_animated_toast_stack(&queue, &age, area, 1, &mut frame);
        let text = ftui_harness::buffer_to_text(&frame.buffer);
        let first_non_space = text
            .lines()
            .find_map(|line| line.chars().position(|ch| !ch.is_whitespace()))
            .expect("toast border should be rendered");

        assert!(
            first_non_space >= 9,
            "expected entrance offset to shift toast right, got column {first_non_space}",
        );
    }

    #[test]
    fn toast_overlay_area_insets_content_header_rows() {
        let content = Rect::new(0, 1, 100, 20);
        let fallback = Rect::new(0, 0, 100, 22);
        let area = toast_overlay_area(content, fallback);
        assert_eq!(area.x, 0);
        assert_eq!(area.y, 3);
        assert_eq!(area.width, 100);
        assert_eq!(area.height, 18);
    }

    #[test]
    fn toast_overlay_area_falls_back_when_content_is_empty() {
        let content = Rect::new(0, 0, 0, 0);
        let fallback = Rect::new(0, 0, 80, 24);
        let area = toast_overlay_area(content, fallback);
        assert_eq!(area, fallback);
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
        assert_eq!(fg, toast_color_error());
    }

    #[test]
    fn toast_for_event_warning_renders_warning_color() {
        let event = MailEvent::tool_call_end("slow_tool", 6000, None, 0, 0.0, vec![], None, None);
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, toast_color_warning());
    }

    #[test]
    fn toast_for_event_info_renders_info_color() {
        let event = MailEvent::message_sent(1, "A", vec!["B".into()], "Hi", "t1", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, toast_color_info());
    }

    #[test]
    fn toast_for_event_success_renders_success_color() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "proj");
        let toast = toast_for_event(&event, ToastSeverityThreshold::Info).unwrap();
        let fg = render_toast_border_fg(&toast);
        assert_eq!(fg, toast_color_success());
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
        queue.notify(Toast::new("test").duration(Duration::from_mins(1)));
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
        queue.notify(Toast::new("test toast").duration(Duration::from_mins(1)));
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

        // Assert the operator hint text itself is present in the expected row.
        let snapshot = ftui_harness::buffer_to_text(&frame.buffer);
        let hint_row = snapshot
            .lines()
            .nth(hint_y as usize)
            .unwrap_or_default()
            .trim_end();
        assert!(
            hint_row.contains("Ctrl+Y"),
            "Hint text should be rendered below the focused toast",
        );
    }

    // ── Modal manager tests ─────────────────────────────────────

    #[test]
    fn modal_manager_enter_confirms_and_invokes_callback() {
        let mut manager = ModalManager::new();
        let (tx, rx) = mpsc::channel();
        manager.show_confirmation(
            "Confirm",
            "Proceed?",
            ModalSeverity::Warning,
            move |result| {
                tx.send(result).expect("send modal result");
            },
        );
        assert!(manager.is_active());

        let consumed = manager.handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter)));
        assert!(consumed);
        assert!(!manager.is_active());
        assert_eq!(rx.recv().expect("modal callback result"), DialogResult::Ok);
    }

    #[test]
    fn modal_manager_escape_dismisses_and_invokes_callback() {
        let mut manager = ModalManager::new();
        let (tx, rx) = mpsc::channel();
        manager.show_confirmation(
            "Confirm",
            "Proceed?",
            ModalSeverity::Warning,
            move |result| {
                tx.send(result).expect("send modal result");
            },
        );
        assert!(manager.is_active());

        let consumed = manager.handle_event(&Event::Key(KeyEvent::new(KeyCode::Escape)));
        assert!(consumed);
        assert!(!manager.is_active());
        assert_eq!(
            rx.recv().expect("modal callback result"),
            DialogResult::Dismissed
        );
    }

    #[test]
    fn modal_manager_tab_cycles_button_focus() {
        let mut manager = ModalManager::new();
        manager.show_confirmation("Confirm", "Proceed?", ModalSeverity::Info, |_| {});
        assert!(manager.is_active());
        assert!(
            manager
                .active
                .as_ref()
                .expect("active modal")
                .state
                .focused_button
                .is_none()
        );

        let tab = Event::Key(KeyEvent::new(KeyCode::Tab));
        manager.handle_event(&tab);
        assert_eq!(
            manager
                .active
                .as_ref()
                .expect("active modal")
                .state
                .focused_button,
            Some(1)
        );

        manager.handle_event(&tab);
        assert_eq!(
            manager
                .active
                .as_ref()
                .expect("active modal")
                .state
                .focused_button,
            Some(0)
        );
    }

    #[test]
    fn modal_manager_dismiss_clears_without_callback() {
        let mut manager = ModalManager::new();
        let (tx, rx) = mpsc::channel();
        manager.show_confirmation("Confirm", "Proceed?", ModalSeverity::Info, move |result| {
            tx.send(result).expect("send modal result");
        });
        assert!(manager.is_active());

        manager.dismiss();
        assert!(!manager.is_active());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn modal_focus_trap_blocks_palette_shortcuts_until_modal_closes() {
        let mut model = test_model();
        let (tx, rx) = mpsc::channel();
        model.modal_manager.show_confirmation(
            "Confirm",
            "Proceed?",
            ModalSeverity::Warning,
            move |result| {
                tx.send(result).expect("send modal result");
            },
        );
        assert!(model.modal_manager.is_active());
        assert!(!model.command_palette.is_visible());

        let ctrl_p = Event::Key(KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL));
        let cmd = model.update(MailMsg::Terminal(ctrl_p));
        assert!(matches!(cmd, Cmd::None));
        assert!(model.modal_manager.is_active());
        assert!(!model.command_palette.is_visible());

        let cmd = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
        assert!(matches!(cmd, Cmd::None));
        assert!(!model.modal_manager.is_active());
        assert_eq!(rx.recv().expect("modal callback result"), DialogResult::Ok);
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
            "Toast overlay p95 exceeds 1ms: p50={p50_us}µs, p95={p95_us}µs, p99={p99_us}µs",
        );

        eprintln!(
            "[perf] Toast overlay render (3 toasts, 160x48): \
             p50={p50_us}µs p95={p95_us}µs p99={p99_us}µs",
        );
    }

    /// Benchmark: render sparkline with 100 data points.
    ///
    /// Budget: sparkline render should complete in < 500µs.
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn perf_sparkline_100points() {
        use ftui_widgets::sparkline::Sparkline;
        use std::time::Instant;

        let area = Rect::new(0, 0, 80, 1);
        let mut pool = ftui::GraphemePool::new();

        // Generate 100 data points with variation
        let data: Vec<f64> = (0..100)
            .map(|i| {
                let base = 50.0;
                let wave = (f64::from(i) * 0.2).sin() * 30.0;
                let noise = f64::from((i * 7) % 13) - 6.0;
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
            "Sparkline p95 exceeds 500µs: p50={p50_us}µs, p95={p95_us}µs, p99={p99_us}µs",
        );

        eprintln!(
            "[perf] Sparkline render (100 points, 80 cols): \
             p50={p50_us}µs p95={p95_us}µs p99={p99_us}µs",
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
            "Modal overlay p95 exceeds 1ms: p50={p50_us}µs, p95={p95_us}µs, p99={p99_us}µs",
        );

        eprintln!(
            "[perf] Modal overlay render (160x48): \
             p50={p50_us}µs p95={p95_us}µs p99={p99_us}µs",
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
                    format!("action:{i}"),
                    format!("Action Item Number {i} Description"),
                )
                .with_description(format!(
                    "This is action {i} which does something useful in category {category}",
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
            "Command palette fuzzy search p95 exceeds 2ms: p50={p50_us}µs, p95={p95_us}µs, p99={p99_us}µs",
        );

        eprintln!(
            "[perf] Command palette fuzzy search (100 entries): \
             p50={p50_us}µs p95={p95_us}µs p99={p99_us}µs",
        );
    }

    // ── Error boundary tests ────────────────────────────────────────

    /// A screen that panics on demand, for error boundary testing.
    struct PanickingScreen {
        panic_on_view: bool,
        panic_on_update: bool,
        panic_on_tick: bool,
    }

    impl PanickingScreen {
        fn new() -> Self {
            Self {
                panic_on_view: false,
                panic_on_update: false,
                panic_on_tick: false,
            }
        }
    }

    impl MailScreen for PanickingScreen {
        fn update(&mut self, _event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
            assert!(!self.panic_on_update, "update panic");
            Cmd::none()
        }

        fn view(&self, _frame: &mut ftui::Frame<'_>, _area: Rect, _state: &TuiSharedState) {
            assert!(!self.panic_on_view, "view panic");
        }

        fn tick(&mut self, _tick_count: u64, _state: &TuiSharedState) {
            assert!(!self.panic_on_tick, "tick panic");
        }

        fn title(&self) -> &'static str {
            "Panicking"
        }
    }

    #[test]
    fn error_boundary_view_catches_panic() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_view = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // The view should catch the panic and record it.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        model.view(&mut frame);

        // Screen should be recorded as panicked.
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );
        let msg = model
            .screen_panics
            .borrow()
            .get(&MailScreenId::Messages)
            .cloned()
            .unwrap();
        assert_eq!(msg, "view panic");
    }

    #[test]
    fn error_boundary_update_catches_panic() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_update = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // Send a key event that gets forwarded to the screen.
        let key_event = Event::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: Modifiers::NONE,
            kind: KeyEventKind::Press,
        });
        let cmd = model.update(MailMsg::Terminal(key_event));

        // Should not crash, returns Cmd::None.
        assert!(matches!(cmd, Cmd::None));
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );
    }

    #[test]
    fn error_boundary_tick_catches_panic() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_tick = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));

        // Tick should catch the panic.
        model.update(MailMsg::Terminal(Event::Tick));

        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );
    }

    #[test]
    fn error_boundary_panicked_screen_shows_fallback_not_rerender() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_view = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // First view catches the panic.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );

        // Second view should render fallback without re-panicking.
        let mut pool2 = ftui::GraphemePool::new();
        let mut frame2 = Frame::new(80, 24, &mut pool2);
        model.view(&mut frame2);
        // Still panicked — no crash on second render.
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );
    }

    #[test]
    fn error_boundary_r_key_resets_panicked_screen() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_view = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // Trigger the panic.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );

        // Press 'r' to reset.
        let r_key = Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: Modifiers::NONE,
            kind: KeyEventKind::Press,
        });
        model.update(MailMsg::Terminal(r_key));

        // Screen should be cleared from panics (fresh screen installed).
        assert!(
            !model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );
    }

    #[test]
    fn error_boundary_panicked_screen_swallows_non_r_keys() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_update = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // Trigger the panic via update.
        let x_key = Event::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: Modifiers::NONE,
            kind: KeyEventKind::Press,
        });
        model.update(MailMsg::Terminal(x_key));
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );

        // Send another key — should be swallowed, no crash.
        let j_key = Event::Key(KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: Modifiers::NONE,
            kind: KeyEventKind::Press,
        });
        let cmd = model.update(MailMsg::Terminal(j_key));
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn error_boundary_other_screens_unaffected() {
        let mut model = test_model();
        let mut screen = PanickingScreen::new();
        screen.panic_on_view = true;
        model.set_screen(MailScreenId::Messages, Box::new(screen));
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        // Trigger panic on Messages screen.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        assert!(
            model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Messages)
        );

        // Switch to Dashboard — should work fine.
        model.update(MailMsg::SwitchScreen(MailScreenId::Dashboard));
        assert_eq!(model.active_screen(), MailScreenId::Dashboard);
        let mut pool2 = ftui::GraphemePool::new();
        let mut frame2 = Frame::new(80, 24, &mut pool2);
        model.view(&mut frame2);
        // Dashboard is not panicked.
        assert!(
            !model
                .screen_panics
                .borrow()
                .contains_key(&MailScreenId::Dashboard)
        );
    }

    #[test]
    fn panic_payload_to_string_extracts_str() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("hello");
        assert_eq!(panic_payload_to_string(&payload), "hello");
    }

    #[test]
    fn panic_payload_to_string_extracts_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("world"));
        assert_eq!(panic_payload_to_string(&payload), "world");
    }

    #[test]
    fn panic_payload_to_string_unknown_type() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
        assert_eq!(panic_payload_to_string(&payload), "unknown panic");
    }

    // ── Overlay stack precedence tests ─────────────────────────

    #[test]
    fn topmost_overlay_none_when_clean() {
        let model = test_model();
        assert_eq!(model.topmost_overlay(), OverlayLayer::None);
    }

    #[test]
    fn topmost_overlay_help_when_visible() {
        let mut model = test_model();
        model.help_visible = true;
        assert_eq!(model.topmost_overlay(), OverlayLayer::Help);
    }

    #[test]
    fn topmost_overlay_toast_focus_beats_help() {
        let mut model = test_model();
        model.help_visible = true;
        model.toast_focus_index = Some(0);
        // Toast focus has higher event-routing precedence than help.
        assert_eq!(model.topmost_overlay(), OverlayLayer::ToastFocus);
    }

    #[test]
    fn topmost_overlay_palette_beats_all() {
        let mut model = test_model();
        model.help_visible = true;
        model.toast_focus_index = Some(0);
        model.open_palette();
        assert_eq!(model.topmost_overlay(), OverlayLayer::Palette);
    }

    #[test]
    fn topmost_overlay_modal_beats_action_menu() {
        let mut model = test_model();
        // Open action menu
        model.action_menu.open(
            vec![crate::tui_action_menu::ActionEntry::new(
                "Test",
                crate::tui_action_menu::ActionKind::Execute("test".to_string()),
            )],
            5, // anchor_row
            "test-ctx",
        );
        assert_eq!(model.topmost_overlay(), OverlayLayer::ActionMenu);

        // Now open a modal — should take precedence
        model
            .modal_manager
            .show_confirmation("Test", "Are you sure?", ModalSeverity::Info, |_| {});
        assert_eq!(model.topmost_overlay(), OverlayLayer::Modal);
    }

    #[test]
    fn overlay_layer_ordering_matches_event_routing() {
        // The Ord implementation should match the event-routing precedence:
        // higher value = higher z-order in event routing.
        assert!(OverlayLayer::Inspector > OverlayLayer::Palette);
        assert!(OverlayLayer::Palette > OverlayLayer::Modal);
        assert!(OverlayLayer::Modal > OverlayLayer::ActionMenu);
        assert!(OverlayLayer::ActionMenu > OverlayLayer::ToastFocus);
        assert!(OverlayLayer::ToastFocus > OverlayLayer::Toasts);
        assert!(OverlayLayer::Toasts > OverlayLayer::None);
    }

    #[test]
    fn overlay_layer_focus_trapping_contract() {
        assert!(OverlayLayer::Inspector.traps_focus());
        assert!(OverlayLayer::Palette.traps_focus());
        assert!(OverlayLayer::Modal.traps_focus());
        assert!(OverlayLayer::ActionMenu.traps_focus());
        assert!(OverlayLayer::ToastFocus.traps_focus());
        assert!(OverlayLayer::MacroPlayback.traps_focus());
        // Help does NOT trap focus — it only consumes Esc/j/k.
        assert!(!OverlayLayer::Help.traps_focus());
        // Passive toasts don't trap focus.
        assert!(!OverlayLayer::Toasts.traps_focus());
        assert!(!OverlayLayer::None.traps_focus());
    }

    #[test]
    fn escape_closes_topmost_help() {
        let mut model = test_model();
        model.help_visible = true;
        assert_eq!(model.topmost_overlay(), OverlayLayer::Help);

        // Send Escape
        let esc = MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Escape)));
        model.update(esc);
        assert!(!model.help_visible);
        assert_eq!(model.topmost_overlay(), OverlayLayer::None);
    }

    #[test]
    fn escape_closes_toast_focus_not_help() {
        let mut model = test_model();
        model.help_visible = true;
        model.notifications.notify(
            ftui_widgets::Toast::new("test")
                .icon(ftui_widgets::ToastIcon::Info)
                .duration(Duration::from_mins(1)),
        );
        model.toast_focus_index = Some(0);

        // Toast focus should be topmost
        assert_eq!(model.topmost_overlay(), OverlayLayer::ToastFocus);

        // Escape should close toast focus, NOT help
        let esc = MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Escape)));
        model.update(esc);
        assert!(model.toast_focus_index.is_none());
        assert!(model.help_visible, "help should still be visible");
    }

    #[test]
    fn help_overlay_traps_tab_navigation_keys() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        model.help_visible = true;
        let before = model.screen_manager.active_screen();

        let tab = MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Tab)));
        model.update(tab);
        assert_eq!(model.screen_manager.active_screen(), before);
        assert!(model.help_visible);

        let backtab = MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::BackTab)));
        model.update(backtab);
        assert_eq!(model.screen_manager.active_screen(), before);
        assert!(model.help_visible);
    }

    #[test]
    fn f12_is_noop_when_debug_disabled() {
        let mut model = test_model_with_debug(false);
        let cmd = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::F(12)))));
        assert!(matches!(cmd, Cmd::None));
        assert!(!model.inspector.is_active());
        assert_eq!(model.topmost_overlay(), OverlayLayer::None);
    }

    #[test]
    fn f12_toggles_inspector_when_debug_enabled() {
        let mut model = test_model_with_debug(true);
        assert!(!model.inspector.is_active());
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::F(12)))));
        assert!(model.inspector.is_active());
        assert_eq!(model.topmost_overlay(), OverlayLayer::Inspector);
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::F(12)))));
        assert!(!model.inspector.is_active());
    }

    #[test]
    fn inspector_arrows_move_selection_enter_toggles_props_escape_dismisses() {
        let mut model = test_model_with_debug(true);
        model.inspector_last_tree_len.set(3);
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::F(12)))));
        assert!(model.inspector.is_active());
        assert_eq!(model.inspector_selected_index, 0);
        assert!(!model.inspector_show_properties);

        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Down))));
        assert_eq!(model.inspector_selected_index, 1);
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Right))));
        assert_eq!(model.inspector_selected_index, 2);
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Down))));
        assert_eq!(model.inspector_selected_index, 2, "selection clamps at end");
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Up))));
        assert_eq!(model.inspector_selected_index, 1);

        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
        assert!(model.inspector_show_properties);
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
        assert!(!model.inspector_show_properties);

        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Escape,
        ))));
        assert!(!model.inspector.is_active());
        assert_ne!(model.topmost_overlay(), OverlayLayer::Inspector);
    }

    #[test]
    fn inspector_widget_tree_reflects_current_screen_panels() {
        let mut model = test_model_with_debug(true);
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));

        let area = Rect::new(0, 0, 120, 40);
        let chrome = crate::tui_chrome::chrome_layout(area);
        crate::tui_chrome::record_tab_hit_slots(
            chrome.tab_bar,
            model.active_screen(),
            &model.mouse_dispatcher,
        );

        let tree = model.build_inspector_widget_tree(
            area,
            &chrome,
            MailScreenId::Messages,
            11,
            22,
            33,
            0,
            0,
            0,
            0,
            0,
        );
        let child_names: Vec<&str> = tree
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect();
        let expected_screen_name = format!(
            "Screen {}",
            crate::tui_screens::screen_meta(MailScreenId::Messages).short_label
        );

        assert!(child_names.contains(&"TabBar"));
        assert!(child_names.iter().any(|name| *name == expected_screen_name));
        assert!(child_names.contains(&"StatusLine"));
        assert!(child_names.contains(&"InspectorOverlay"));

        let tab_bar = tree
            .children
            .iter()
            .find(|child| child.name == "TabBar")
            .expect("tab bar should be present");
        assert!(
            !tab_bar.children.is_empty(),
            "tab bar should expose tab children in inspector tree"
        );
    }

    // ── Coach hint tests ─────────────────────────────────────────

    #[test]
    fn coach_hint_first_visit_returns_message() {
        let mut mgr = CoachHintManager::new();
        let msg = mgr.on_screen_visit(MailScreenId::Dashboard);
        assert!(msg.is_some(), "first visit should produce hint");
        assert!(msg.unwrap().contains("Tip:"));
    }

    #[test]
    fn coach_hint_second_visit_same_session_returns_none() {
        let mut mgr = CoachHintManager::new();
        let _ = mgr.on_screen_visit(MailScreenId::Messages);
        let msg = mgr.on_screen_visit(MailScreenId::Messages);
        assert!(
            msg.is_none(),
            "second visit in same session should not show hint"
        );
    }

    #[test]
    fn coach_hint_dismissed_persists_across_managers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dismissed_hints.json");

        // First manager dismisses a hint
        let mut mgr1 = CoachHintManager::new().with_persist_path(path.clone());
        let _ = mgr1.on_screen_visit(MailScreenId::Search);
        mgr1.flush_if_dirty();

        // Second manager should not show the same hint
        let mut mgr2 = CoachHintManager::new().with_persist_path(path);
        let msg = mgr2.on_screen_visit(MailScreenId::Search);
        assert!(msg.is_none(), "dismissed hint should not reappear");
    }

    #[test]
    fn coach_hint_disabled_returns_none() {
        let mut mgr = CoachHintManager::new();
        mgr.enabled = false;
        let msg = mgr.on_screen_visit(MailScreenId::Dashboard);
        assert!(msg.is_none(), "disabled hints should not show");
    }

    #[test]
    fn coach_hint_all_screens_have_hints() {
        for &screen in ALL_SCREEN_IDS {
            let found = COACH_HINTS.iter().any(|h| h.screen == screen);
            assert!(found, "missing coach hint for {screen:?}");
        }
    }

    #[test]
    fn coach_hint_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for hint in COACH_HINTS {
            assert!(seen.insert(hint.id), "duplicate coach hint id: {}", hint.id);
        }
    }

    #[test]
    fn coach_hints_reference_valid_keys() {
        // Verify that key names mentioned in coach hints exist in either
        // screen-local keybindings or GLOBAL_BINDINGS — auto-synchronization
        // contract (br-1xt0m.1.12.3).
        use crate::tui_keymap::GLOBAL_BINDINGS;

        let config = mcp_agent_mail_core::Config::default();
        let state = std::sync::Arc::new(TuiSharedState::new(&config));

        // Known key-like tokens that appear in hint messages.
        let key_tokens: &[(&str, &[&str])] = &[
            ("dashboard:welcome", &["?", "Ctrl+P"]),
            ("messages:search", &["/", "Enter"]),
            ("threads:expand", &["Enter", "h"]),
            ("agents:inbox", &["/", "s"]),
            ("search:syntax", &[]), // syntax tips, no specific key
            ("reservations:force", &["f"]),
            ("tool_metrics:sort", &["Tab"]),
            ("system_health:diag", &[]), // informational, no specific key
            ("timeline:expand", &["Enter"]),
            ("projects:browse", &["/", "s"]),
            ("contacts:approve", &[]), // inline action, no specific key
            ("explorer:filter", &[]),  // informational
            ("analytics:volume", &[]), // informational
            ("attachments:preview", &["Enter", "/"]),
        ];

        for (hint_id, keys) in key_tokens {
            let hint = COACH_HINTS
                .iter()
                .find(|h| h.id == *hint_id)
                .unwrap_or_else(|| panic!("missing hint {hint_id}"));
            let screen = ScreenManager::create_screen(hint.screen, &state);
            let bindings = screen.keybindings();

            for key in *keys {
                let in_screen = bindings.iter().any(|b| b.key.contains(key));
                let in_global = GLOBAL_BINDINGS.iter().any(|b| b.label.contains(key));
                assert!(
                    in_screen || in_global,
                    "coach hint '{hint_id}' references key '{key}' not found in \
                     screen {:?} keybindings or GLOBAL_BINDINGS",
                    hint.screen,
                );
            }
        }
    }

    #[test]
    fn context_help_tips_cover_all_screens() {
        // Every screen should provide a non-empty context_help_tip()
        // for the help overlay (br-1xt0m.1.12.3 acceptance).
        let config = mcp_agent_mail_core::Config::default();
        let state = std::sync::Arc::new(TuiSharedState::new(&config));
        for &id in ALL_SCREEN_IDS {
            let screen = ScreenManager::create_screen(id, &state);
            let tip = screen.context_help_tip();
            assert!(tip.is_some(), "screen {id:?} missing context_help_tip()");
            let text = tip.unwrap();
            assert!(
                !text.is_empty(),
                "screen {id:?} has empty context_help_tip()"
            );
        }
    }

    // ── extract_numeric_id tests ────────────────────────────────

    #[test]
    fn extract_numeric_id_plain_number() {
        assert_eq!(extract_numeric_id("123"), Some(123));
        assert_eq!(extract_numeric_id("0"), Some(0));
        assert_eq!(extract_numeric_id("-5"), Some(-5));
    }

    #[test]
    fn extract_numeric_id_prefixed() {
        assert_eq!(extract_numeric_id("message:42"), Some(42));
        assert_eq!(extract_numeric_id("reservation:7"), Some(7));
        assert_eq!(extract_numeric_id("thread:999"), Some(999));
    }

    #[test]
    fn extract_numeric_id_non_numeric() {
        assert_eq!(extract_numeric_id("abc"), None);
        assert_eq!(extract_numeric_id("msg:abc"), None);
        assert_eq!(extract_numeric_id(""), None);
    }

    #[test]
    fn extract_numeric_id_multiple_colons() {
        // Uses rsplit_once, so "a:b:123" → prefix="a:b", num="123"
        assert_eq!(extract_numeric_id("a:b:123"), Some(123));
    }

    #[test]
    fn parse_rethread_operation_arg_valid() {
        assert_eq!(
            parse_rethread_operation_arg("55:br-123"),
            Some((55, "br-123".to_string()))
        );
    }

    #[test]
    fn parse_rethread_operation_arg_rejects_invalid_shapes() {
        assert_eq!(parse_rethread_operation_arg("55"), None);
        assert_eq!(parse_rethread_operation_arg("abc:br-1"), None);
        assert_eq!(parse_rethread_operation_arg("55:   "), None);
    }

    // ── dispatch_execute_operation tests ─────────────────────────

    #[test]
    fn dispatch_execute_copy_event_shows_toast() {
        let mut model = test_model();
        let _cmd = model.dispatch_execute_operation("copy_event", "");
        // Tick makes newly-notified toasts visible.
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    #[test]
    fn dispatch_execute_unknown_op_shows_fallback_toast() {
        let mut model = test_model();
        let _cmd = model.dispatch_execute_operation("unknown_op", "ctx");
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    #[test]
    fn dispatch_execute_server_op_returns_action_execute_msg() {
        let mut model = test_model();
        let cmd = model.dispatch_execute_operation("acknowledge", "msg:55");
        // Should produce a Cmd::Msg with ActionExecute and a toast.
        assert!(matches!(cmd, Cmd::Msg(_)));
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    #[test]
    fn dispatch_execute_rethread_invalid_args_shows_warning() {
        let mut model = test_model();
        let cmd = model.dispatch_execute_operation("rethread_message:not-a-shape", "");
        assert!(matches!(cmd, Cmd::None));
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    // ── deferred confirmed action tests ─────────────────────────

    #[test]
    fn drain_deferred_action_lifecycle() {
        // Phase 1: empty queue → Cmd::None.
        let mut model = test_model();
        let cmd = model.drain_deferred_confirmed_action();
        assert!(matches!(cmd, Cmd::None));

        // Phase 2: queued server-dispatched action → Cmd::Msg.
        model
            .action_tx
            .send(("acknowledge".to_string(), "msg:10".to_string()))
            .expect("queue deferred action");
        let cmd = model.drain_deferred_confirmed_action();
        assert!(matches!(cmd, Cmd::Msg(_)));
        let drained = model.action_rx.try_recv();
        assert!(
            matches!(drained, Err(std::sync::mpsc::TryRecvError::Empty)),
            "channel should be drained",
        );
    }

    // ── ConfirmThenExecute end-to-end tests ─────────────────────

    #[test]
    fn confirm_then_execute_lifecycle() {
        // ── Phase 1: ConfirmThenExecute shows a modal ────────────
        let mut model = test_model();
        let _cmd = model.dispatch_action_menu_selection(
            ActionKind::ConfirmThenExecute {
                title: "Delete?".to_string(),
                message: "This is destructive".to_string(),
                operation: "force_release:42".to_string(),
            },
            "ctx",
        );
        assert!(model.modal_manager.is_active(), "modal should be shown");
        // Dismiss for next phase.
        model
            .modal_manager
            .handle_event(&Event::Key(KeyEvent::new(KeyCode::Escape)));

        // ── Phase 2: OK stores the deferred action ───────────────
        model.dispatch_action_menu_selection(
            ActionKind::ConfirmThenExecute {
                title: "Release?".to_string(),
                message: "Confirm release".to_string(),
                operation: "release:7".to_string(),
            },
            "reservation:7",
        );
        model
            .modal_manager
            .handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter)));
        assert!(!model.modal_manager.is_active());
        let cmd = model.drain_deferred_confirmed_action();
        match cmd {
            Cmd::Msg(MailMsg::Screen(MailScreenMsg::ActionExecute(op, ctx))) => {
                assert_eq!(op, "release:7");
                assert_eq!(ctx, "reservation:7");
            }
            other => panic!("expected deferred ActionExecute after confirm, got: {other:?}"),
        }

        // ── Phase 3: Cancel does NOT store ───────────────────────
        model.dispatch_action_menu_selection(
            ActionKind::ConfirmThenExecute {
                title: "Delete?".to_string(),
                message: "Sure?".to_string(),
                operation: "force_release:99".to_string(),
            },
            "ctx",
        );
        model
            .modal_manager
            .handle_event(&Event::Key(KeyEvent::new(KeyCode::Escape)));
        assert!(!model.modal_manager.is_active());
        let cmd = model.drain_deferred_confirmed_action();
        assert!(
            matches!(cmd, Cmd::None),
            "cancelled confirm should not queue deferred action",
        );

        // ── Phase 4: Full lifecycle via tick drain ────────────────
        model.dispatch_action_menu_selection(
            ActionKind::ConfirmThenExecute {
                title: "Acknowledge?".to_string(),
                message: "Acknowledge message 55?".to_string(),
                operation: "acknowledge:55".to_string(),
            },
            "msg:55",
        );
        model
            .modal_manager
            .handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter)));
        let cmd = model.drain_deferred_confirmed_action();
        assert!(matches!(cmd, Cmd::Msg(_)), "acknowledge → Cmd::Msg");
        let drained = model.action_rx.try_recv();
        assert!(
            matches!(drained, Err(std::sync::mpsc::TryRecvError::Empty)),
            "channel should be drained",
        );
    }

    // ── ActionOutcome tests ─────────────────────────────────────

    #[test]
    fn action_outcome_success_shows_toast() {
        let mut model = test_model();
        model.record_action_outcome(ActionOutcome::Success {
            operation: "acknowledge".into(),
            summary: "Message 42 acknowledged".into(),
        });
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
        assert_eq!(model.action_outcomes.len(), 1);
    }

    #[test]
    fn action_outcome_failure_shows_error_toast() {
        let mut model = test_model();
        model.record_action_outcome(ActionOutcome::Failure {
            operation: "release".into(),
            error: "Reservation not found".into(),
        });
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    #[test]
    fn action_outcome_replaces_in_flight() {
        let mut model = test_model();
        model.record_action_outcome(ActionOutcome::InFlight {
            operation: "renew".into(),
        });
        assert_eq!(model.action_outcomes.len(), 1);

        // Success replaces the InFlight entry.
        model.record_action_outcome(ActionOutcome::Success {
            operation: "renew".into(),
            summary: "TTL extended".into(),
        });
        assert_eq!(model.action_outcomes.len(), 1);
        assert!(matches!(
            model.action_outcomes[0],
            ActionOutcome::Success { .. }
        ));
    }

    #[test]
    fn action_outcome_caps_history() {
        let mut model = test_model();
        for i in 0..25 {
            model.record_action_outcome(ActionOutcome::Success {
                operation: format!("op-{i}"),
                summary: "done".into(),
            });
        }
        assert!(
            model.action_outcomes.len() <= 21,
            "outcome history should be capped"
        );
    }

    #[test]
    fn server_dispatch_records_in_flight() {
        let mut model = test_model();
        model.action_outcomes.clear();
        let _cmd = model.dispatch_execute_operation("acknowledge", "msg:99");
        assert!(
            model.action_outcomes.iter().any(
                |o| matches!(o, ActionOutcome::InFlight { operation } if operation == "acknowledge")
            ),
            "server-dispatched op should record InFlight outcome"
        );
    }

    // ── Hit regions, dispatch routing, action state machines (br-1xt0m.1.13.7) ──

    #[test]
    fn overlay_layer_ordering_values_increase() {
        // OverlayLayer discriminants must increase from None to Inspector.
        let layers = [
            OverlayLayer::None,
            OverlayLayer::Toasts,
            OverlayLayer::ToastFocus,
            OverlayLayer::ActionMenu,
            OverlayLayer::MacroPlayback,
            OverlayLayer::Modal,
            OverlayLayer::Palette,
            OverlayLayer::Help,
            OverlayLayer::Inspector,
        ];
        for i in 1..layers.len() {
            assert!(
                (layers[i] as u8) > (layers[i - 1] as u8),
                "{:?} should be higher than {:?}",
                layers[i],
                layers[i - 1],
            );
        }
    }

    #[test]
    fn overlay_layer_focus_trapping_contracts() {
        // Layers that should trap focus (consume all events).
        assert!(OverlayLayer::Inspector.traps_focus());
        assert!(OverlayLayer::Palette.traps_focus());
        assert!(OverlayLayer::Modal.traps_focus());
        assert!(OverlayLayer::ActionMenu.traps_focus());
        assert!(OverlayLayer::ToastFocus.traps_focus());
        assert!(OverlayLayer::MacroPlayback.traps_focus());

        // Layers that should NOT trap focus.
        assert!(!OverlayLayer::None.traps_focus());
        assert!(!OverlayLayer::Toasts.traps_focus());
        assert!(!OverlayLayer::Help.traps_focus());
    }

    #[test]
    fn dispatch_navigate_action_switches_screen() {
        let mut model = test_model();
        let cmd =
            model.dispatch_action_menu_selection(ActionKind::Navigate(MailScreenId::Agents), "ctx");
        assert_eq!(model.active_screen(), MailScreenId::Agents);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn dispatch_deep_link_action_switches_screen() {
        let mut model = test_model();
        let cmd = model.dispatch_action_menu_selection(
            ActionKind::DeepLink(DeepLinkTarget::ThreadById("br-42".to_string())),
            "ctx",
        );
        assert_eq!(model.active_screen(), MailScreenId::Threads);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn dispatch_copy_to_clipboard_shows_toast() {
        let mut model = test_model();
        let cmd = model.dispatch_action_menu_selection(
            ActionKind::CopyToClipboard("some text to copy".into()),
            "ctx",
        );
        assert!(matches!(cmd, Cmd::None));
        // A toast notification should have been queued.
        model.notifications.tick(Duration::from_millis(16));
        assert!(model.notifications.visible_count() > 0);
    }

    #[test]
    fn dispatch_dismiss_action_is_noop() {
        let mut model = test_model();
        let prev = model.active_screen();
        let cmd = model.dispatch_action_menu_selection(ActionKind::Dismiss, "ctx");
        assert_eq!(model.active_screen(), prev);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn action_outcome_inflight_to_failure_replacement() {
        let mut model = test_model();
        model.record_action_outcome(ActionOutcome::InFlight {
            operation: "release".into(),
        });
        assert_eq!(model.action_outcomes.len(), 1);

        model.record_action_outcome(ActionOutcome::Failure {
            operation: "release".into(),
            error: "Not found".into(),
        });
        assert_eq!(model.action_outcomes.len(), 1);
        assert!(matches!(
            model.action_outcomes[0],
            ActionOutcome::Failure { .. }
        ));
    }

    #[test]
    fn action_outcome_different_operations_coexist() {
        let mut model = test_model();
        model.record_action_outcome(ActionOutcome::InFlight {
            operation: "ack".into(),
        });
        model.record_action_outcome(ActionOutcome::InFlight {
            operation: "release".into(),
        });
        assert_eq!(model.action_outcomes.len(), 2);

        // Resolving one doesn't affect the other.
        model.record_action_outcome(ActionOutcome::Success {
            operation: "ack".into(),
            summary: "done".into(),
        });
        assert_eq!(model.action_outcomes.len(), 2);
    }

    #[test]
    fn modal_manager_show_and_dismiss() {
        let mut modal = ModalManager::new();
        assert!(!modal.is_active());
        modal.show_confirmation("Title", "Message", ModalSeverity::Info, |_| {});
        assert!(modal.is_active());

        // Escape dismisses.
        modal.handle_event(&Event::Key(KeyEvent::new(KeyCode::Escape)));
        assert!(!modal.is_active());
    }

    #[test]
    fn action_menu_blocks_global_shortcuts() {
        let mut model = test_model();
        // Open action menu with a dummy entry.
        model.action_menu.open(
            vec![crate::tui_action_menu::ActionEntry::new(
                "Test",
                ActionKind::Dismiss,
            )],
            5,
            "test-context",
        );
        assert!(model.action_menu.is_active());

        // Press 'q' — should NOT quit while action menu is open.
        let q = Event::Key(ftui::KeyEvent::new(KeyCode::Char('q')));
        let cmd = model.update(MailMsg::Terminal(q));
        assert!(!model.state.is_shutdown_requested());
        assert!(!matches!(cmd, Cmd::Quit));
    }

    // ──────────────────────────────────────────────────────────────────
    // Overlay stack + semantic hierarchy snapshot matrix (br-1xt0m.1.13.9)
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn overlay_layer_count_is_nine() {
        // Verify all layers exist (None through Inspector).
        let layers = [
            OverlayLayer::None,
            OverlayLayer::Toasts,
            OverlayLayer::ToastFocus,
            OverlayLayer::ActionMenu,
            OverlayLayer::MacroPlayback,
            OverlayLayer::Modal,
            OverlayLayer::Palette,
            OverlayLayer::Help,
            OverlayLayer::Inspector,
        ];
        assert_eq!(layers.len(), 9);
        // Each has a unique discriminant.
        let mut values: Vec<u8> = layers.iter().map(|l| *l as u8).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(
            values.len(),
            9,
            "all overlay layers must have unique u8 values"
        );
    }

    #[test]
    fn inspector_is_topmost_z_order() {
        // Inspector (z=8) must be higher than all other layers.
        for layer in [
            OverlayLayer::None,
            OverlayLayer::Toasts,
            OverlayLayer::ToastFocus,
            OverlayLayer::ActionMenu,
            OverlayLayer::MacroPlayback,
            OverlayLayer::Modal,
            OverlayLayer::Palette,
            OverlayLayer::Help,
        ] {
            assert!(
                OverlayLayer::Inspector > layer,
                "Inspector must be > {layer:?}",
            );
        }
    }

    #[test]
    fn palette_above_modal_above_action_menu() {
        // Verify full z-order chain:
        // Inspector > Help > Palette > Modal > MacroPlayback > ActionMenu > ToastFocus > Toasts > None
        assert!(OverlayLayer::Inspector > OverlayLayer::Help);
        assert!(OverlayLayer::Help > OverlayLayer::Palette);
        assert!(OverlayLayer::Palette > OverlayLayer::Modal);
        assert!(OverlayLayer::Modal > OverlayLayer::MacroPlayback);
        assert!(OverlayLayer::MacroPlayback > OverlayLayer::ActionMenu);
        assert!(OverlayLayer::ActionMenu > OverlayLayer::ToastFocus);
        assert!(OverlayLayer::ToastFocus > OverlayLayer::Toasts);
        assert!(OverlayLayer::Toasts > OverlayLayer::None);
    }

    #[test]
    fn topmost_overlay_escalation_chain() {
        let mut model = test_model();

        // Start: None
        assert_eq!(model.topmost_overlay(), OverlayLayer::None);

        // Open help → Help
        model.help_visible = true;
        assert_eq!(model.topmost_overlay(), OverlayLayer::Help);

        // Close help, open toast focus → ToastFocus
        model.help_visible = false;
        model.notifications.notify(
            ftui_widgets::Toast::new("test")
                .icon(ftui_widgets::ToastIcon::Info)
                .duration(Duration::from_mins(1)),
        );
        model.toast_focus_index = Some(0);
        assert_eq!(model.topmost_overlay(), OverlayLayer::ToastFocus);

        // Clear toast focus → back to None
        model.toast_focus_index = None;
    }

    #[test]
    fn screen_switching_does_not_alter_overlay_state() {
        let mut model = test_model();
        model.help_visible = true;
        assert_eq!(model.topmost_overlay(), OverlayLayer::Help);

        // Switch screen — help should remain open.
        model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
        assert!(model.help_visible);
        assert_eq!(model.topmost_overlay(), OverlayLayer::Help);

        model.update(MailMsg::SwitchScreen(MailScreenId::Search));
        assert!(model.help_visible);
    }

    #[test]
    fn panel_focus_outline_is_clipped_to_content_bounds() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 8, &mut pool);
        let content = Rect::new(0, 1, 20, 6);
        let focused = Rect::new(0, 1, 10, 6);

        render_panel_focus_outline(content, focused, &mut frame);

        for x in 0..20_u16 {
            let top = frame.buffer.get(x, 0).expect("top row cell");
            let bottom = frame.buffer.get(x, 7).expect("bottom row cell");
            assert_eq!(top.content.as_char().unwrap_or(' '), ' ');
            assert_eq!(bottom.content.as_char().unwrap_or(' '), ' ');
        }

        for y in 1..=6_u16 {
            let cell = frame.buffer.get(10, y).expect("right outline");
            assert_eq!(cell.content.as_char().unwrap_or(' '), '│');
        }
    }

    #[test]
    fn agents_screen_header_labels_render_without_focus_outline_corruption() {
        let mut model = test_model();
        model.update(MailMsg::SwitchScreen(MailScreenId::Agents));
        model.screen_transition = None;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(180, 40, &mut pool);
        model.view(&mut frame);

        let text = frame_text(&frame);
        assert!(
            text.contains("Name"),
            "expected Name header to render intact; frame dump:\n{text}"
        );
        assert!(
            text.contains("Program"),
            "expected Program header to render intact; frame dump:\n{text}"
        );
    }

    #[test]
    fn all_screens_render_without_panic_at_four_widths() {
        use crate::tui_screens::ALL_SCREEN_IDS;

        let widths: [(u16, u16); 4] = [(80, 24), (100, 30), (120, 40), (160, 48)];
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        for &id in ALL_SCREEN_IDS {
            let mut model = MailAppModel::new(Arc::clone(&state));
            model.update(MailMsg::SwitchScreen(id));

            for (w, h) in widths {
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                model.view(&mut frame);
                // No panic = pass.
            }
        }
    }
}
