//! Theme integration: map ftui theme palettes to TUI-specific styles.
//!
//! Resolves the active `ftui_extras::theme` palette into a
//! [`TuiThemePalette`] struct that every TUI component can query for
//! consistent, theme-aware colors.

use ftui::{PackedRgba, Style, TableTheme};
use ftui_extras::markdown::MarkdownTheme;
use ftui_extras::theme::{self, ThemeId};

use crate::tui_events::{EventSeverity, MailEventKind};

static CUSTOM_THEME_OVERRIDE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// ──────────────────────────────────────────────────────────────────────
// Spacing system
// ──────────────────────────────────────────────────────────────────────

pub const SP_XS: u16 = 1;
pub const SP_SM: u16 = 2;
pub const SP_MD: u16 = 3;
pub const SP_LG: u16 = 4;
pub const SP_XL: u16 = 6;
// Semantic aliases
pub const INLINE_GAP: u16 = SP_XS;
pub const ITEM_GAP: u16 = SP_SM;
pub const PANEL_PADDING: u16 = SP_MD;
pub const SECTION_GAP: u16 = SP_LG;

// ──────────────────────────────────────────────────────────────────────
// TuiThemePalette
// ──────────────────────────────────────────────────────────────────────

/// Resolved TUI color palette derived from the active ftui theme.
///
/// Each field is a concrete `PackedRgba` value ready for use in
/// `Style::default().fg(color)` or `.bg(color)` calls.
#[derive(Debug, Clone, Copy)]
pub struct TuiThemePalette {
    // ── Tab bar ──────────────────────────────────────────────────
    pub tab_active_bg: PackedRgba,
    pub tab_active_fg: PackedRgba,
    pub tab_inactive_bg: PackedRgba,
    pub tab_inactive_fg: PackedRgba,
    pub tab_key_fg: PackedRgba,

    // ── Status line ──────────────────────────────────────────────
    pub status_bg: PackedRgba,
    pub status_fg: PackedRgba,
    pub status_accent: PackedRgba,
    pub status_good: PackedRgba,
    pub status_warn: PackedRgba,

    // ── Help overlay ─────────────────────────────────────────────
    pub help_bg: PackedRgba,
    pub help_fg: PackedRgba,
    pub help_key_fg: PackedRgba,
    pub help_border_fg: PackedRgba,
    pub help_category_fg: PackedRgba,

    // ── Sparkline gradient ───────────────────────────────────────
    pub sparkline_lo: PackedRgba,
    pub sparkline_hi: PackedRgba,

    // ── Table ────────────────────────────────────────────────────
    pub table_header_fg: PackedRgba,
    pub table_row_alt_bg: PackedRgba,

    // ── Selection ────────────────────────────────────────────────
    pub selection_bg: PackedRgba,
    pub selection_fg: PackedRgba,

    // ── Severity ─────────────────────────────────────────────────
    pub severity_ok: PackedRgba,
    pub severity_error: PackedRgba,
    pub severity_warn: PackedRgba,
    pub severity_critical: PackedRgba,

    // ── Panel ────────────────────────────────────────────────────
    pub panel_border: PackedRgba,
    pub panel_border_focused: PackedRgba,
    pub panel_border_dim: PackedRgba,
    pub panel_bg: PackedRgba,
    pub panel_title_fg: PackedRgba,

    // ── Selection extras ─────────────────────────────────────────
    pub selection_indicator: PackedRgba,
    pub list_hover_bg: PackedRgba,

    // ── Data visualization ───────────────────────────────────────
    pub chart_series: [PackedRgba; 6],
    pub chart_axis: PackedRgba,
    pub chart_grid: PackedRgba,

    // ── Badges ───────────────────────────────────────────────────
    pub badge_urgent_bg: PackedRgba,
    pub badge_urgent_fg: PackedRgba,
    pub badge_info_bg: PackedRgba,
    pub badge_info_fg: PackedRgba,

    // ── TTL bands ────────────────────────────────────────────────
    pub ttl_healthy: PackedRgba,
    pub ttl_warning: PackedRgba,
    pub ttl_danger: PackedRgba,
    pub ttl_expired: PackedRgba,

    // ── Metric tiles ─────────────────────────────────────────────
    pub metric_uptime: PackedRgba,
    pub metric_requests: PackedRgba,
    pub metric_latency: PackedRgba,
    pub metric_messages: PackedRgba,
    pub metric_agents: PackedRgba,
    pub metric_ack_ok: PackedRgba,
    pub metric_ack_bad: PackedRgba,

    // ── Agent palette ────────────────────────────────────────────
    pub agent_palette: [PackedRgba; 8],

    // ── Contact status ───────────────────────────────────────────
    pub contact_approved: PackedRgba,
    pub contact_pending: PackedRgba,
    pub contact_blocked: PackedRgba,

    // ── Activity recency ─────────────────────────────────────────
    pub activity_active: PackedRgba,
    pub activity_idle: PackedRgba,
    pub activity_stale: PackedRgba,

    // ── Text / background ────────────────────────────────────────
    pub text_muted: PackedRgba,
    pub text_primary: PackedRgba,
    pub text_secondary: PackedRgba,
    pub text_disabled: PackedRgba,
    pub bg_deep: PackedRgba,
    pub bg_surface: PackedRgba,
    pub bg_overlay: PackedRgba,

    // ── Toast notifications ───────────────────────────────────────
    pub toast_error: PackedRgba,
    pub toast_warning: PackedRgba,
    pub toast_info: PackedRgba,
    pub toast_success: PackedRgba,
    pub toast_focus: PackedRgba,

    // ── JSON token styles ────────────────────────────────────────
    pub json_key: PackedRgba,
    pub json_string: PackedRgba,
    pub json_number: PackedRgba,
    pub json_literal: PackedRgba,
    pub json_punctuation: PackedRgba,
}

impl TuiThemePalette {
    /// Frankenstein's Monster Theme (Showcase)
    #[must_use]
    pub const fn frankenstein() -> Self {
        // Palette:
        // Dark Green BG: 20, 30, 20
        // Electric Green FG: 100, 255, 100
        // Purple Accent: 180, 100, 255
        // Stitch Gray: 100, 100, 100
        // Blood Red: 200, 50, 50

        let bg_deep = PackedRgba::rgb(10, 20, 10);
        let bg_surface = PackedRgba::rgb(20, 40, 20);
        let fg_primary = PackedRgba::rgb(180, 255, 150);
        let fg_muted = PackedRgba::rgb(80, 120, 80);
        let accent = PackedRgba::rgb(180, 80, 220); // Purple stitches
        let warning = PackedRgba::rgb(220, 200, 50); // Lightning yellow
        let _error = PackedRgba::rgb(220, 50, 50); // Blood red (reserved for future use)

        Self {
            tab_active_bg: bg_surface,
            tab_active_fg: fg_primary,
            tab_inactive_bg: bg_deep,
            tab_inactive_fg: fg_muted,
            tab_key_fg: accent,

            status_bg: bg_deep,
            status_fg: fg_primary,
            status_accent: accent,
            status_good: fg_primary,
            status_warn: warning,

            help_bg: bg_deep,
            help_fg: fg_primary,
            help_key_fg: accent,
            help_border_fg: accent,
            help_category_fg: warning,

            sparkline_lo: fg_muted,
            sparkline_hi: fg_primary,

            table_header_fg: accent,
            table_row_alt_bg: bg_surface,

            selection_bg: PackedRgba::rgb(40, 70, 40),
            selection_fg: fg_primary,

            severity_ok: PackedRgba::rgb(80, 220, 100),
            severity_error: PackedRgba::rgb(220, 50, 50),
            severity_warn: warning,
            severity_critical: PackedRgba::rgb(255, 40, 40),

            panel_border: fg_muted,
            panel_border_focused: accent,
            panel_border_dim: PackedRgba::rgb(40, 60, 40),
            panel_bg: bg_deep,
            panel_title_fg: fg_primary,

            selection_indicator: accent,
            list_hover_bg: PackedRgba::rgb(30, 50, 30),

            chart_series: [
                PackedRgba::rgb(80, 220, 100),
                PackedRgba::rgb(100, 180, 255),
                PackedRgba::rgb(255, 184, 108),
                PackedRgba::rgb(255, 100, 150),
                PackedRgba::rgb(180, 80, 220),
                PackedRgba::rgb(220, 200, 50),
            ],
            chart_axis: fg_muted,
            chart_grid: PackedRgba::rgb(30, 50, 30),

            badge_urgent_bg: PackedRgba::rgb(200, 50, 50),
            badge_urgent_fg: PackedRgba::rgb(255, 255, 255),
            badge_info_bg: PackedRgba::rgb(40, 80, 120),
            badge_info_fg: PackedRgba::rgb(180, 220, 255),

            ttl_healthy: PackedRgba::rgb(80, 220, 100),
            ttl_warning: warning,
            ttl_danger: PackedRgba::rgb(220, 80, 50),
            ttl_expired: PackedRgba::rgb(120, 60, 60),

            metric_uptime: PackedRgba::rgb(80, 220, 100),
            metric_requests: PackedRgba::rgb(100, 180, 255),
            metric_latency: PackedRgba::rgb(255, 184, 108),
            metric_messages: PackedRgba::rgb(180, 140, 255),
            metric_agents: PackedRgba::rgb(255, 100, 150),
            metric_ack_ok: PackedRgba::rgb(80, 220, 100),
            metric_ack_bad: PackedRgba::rgb(220, 50, 50),

            agent_palette: [
                PackedRgba::rgb(92, 201, 255),
                PackedRgba::rgb(123, 214, 153),
                PackedRgba::rgb(255, 184, 108),
                PackedRgba::rgb(255, 122, 162),
                PackedRgba::rgb(180, 140, 255),
                PackedRgba::rgb(100, 220, 220),
                PackedRgba::rgb(220, 180, 100),
                PackedRgba::rgb(200, 200, 200),
            ],

            contact_approved: PackedRgba::rgb(80, 220, 100),
            contact_pending: warning,
            contact_blocked: PackedRgba::rgb(220, 50, 50),

            activity_active: PackedRgba::rgb(170, 240, 195),
            activity_idle: PackedRgba::rgb(120, 170, 145),
            activity_stale: PackedRgba::rgb(85, 100, 90),

            text_muted: fg_muted,
            text_primary: fg_primary,
            text_secondary: PackedRgba::rgb(140, 200, 120),
            text_disabled: PackedRgba::rgb(50, 70, 50),
            bg_deep,
            bg_surface,
            bg_overlay: PackedRgba::rgb(30, 55, 30),

            toast_error: PackedRgba::rgb(255, 100, 100),
            toast_warning: PackedRgba::rgb(255, 184, 108),
            toast_info: PackedRgba::rgb(120, 220, 150),
            toast_success: PackedRgba::rgb(100, 220, 170),
            toast_focus: PackedRgba::rgb(80, 220, 255),

            json_key: accent,
            json_string: PackedRgba::rgb(80, 220, 100),
            json_number: PackedRgba::rgb(255, 184, 108),
            json_literal: PackedRgba::rgb(100, 180, 255),
            json_punctuation: fg_muted,
        }
    }

    /// Resolve a palette from a specific theme ID.
    #[must_use]
    pub fn for_theme(id: ThemeId) -> Self {
        let p = theme::palette(id);

        // Tab bar: active uses the surface bg with accent primary highlight.
        // Inactive uses the base bg.
        Self {
            tab_active_bg: p.bg_surface,
            tab_active_fg: p.fg_primary,
            tab_inactive_bg: p.bg_base,
            tab_inactive_fg: p.fg_muted,
            tab_key_fg: p.accent_primary,

            status_bg: p.bg_deep,
            status_fg: p.fg_secondary,
            status_accent: p.accent_primary,
            status_good: p.accent_success,
            status_warn: p.accent_warning,

            help_bg: p.bg_deep,
            help_fg: p.fg_primary,
            help_key_fg: p.accent_primary,
            help_border_fg: p.fg_muted,
            help_category_fg: p.accent_info,

            sparkline_lo: p.accent_secondary,
            sparkline_hi: p.accent_success,

            table_header_fg: p.accent_primary,
            table_row_alt_bg: p.bg_surface,

            selection_bg: p.bg_highlight,
            selection_fg: p.fg_primary,

            severity_ok: p.accent_success,
            severity_error: p.accent_error,
            severity_warn: p.accent_warning,
            severity_critical: p.accent_error,

            panel_border: p.fg_muted,
            panel_border_focused: p.accent_primary,
            panel_border_dim: p.fg_disabled,
            panel_bg: p.bg_deep,
            panel_title_fg: p.fg_primary,

            selection_indicator: p.accent_primary,
            list_hover_bg: p.bg_overlay,

            chart_series: [
                p.accent_success,
                p.accent_info,
                p.accent_warning,
                p.accent_error,
                p.accent_primary,
                p.accent_secondary,
            ],
            chart_axis: p.fg_muted,
            chart_grid: p.bg_surface,

            badge_urgent_bg: p.accent_error,
            badge_urgent_fg: p.fg_primary,
            badge_info_bg: p.bg_overlay,
            badge_info_fg: p.accent_info,

            ttl_healthy: p.accent_success,
            ttl_warning: p.accent_warning,
            ttl_danger: p.accent_error,
            ttl_expired: p.fg_disabled,

            metric_uptime: p.accent_success,
            metric_requests: p.accent_info,
            metric_latency: p.accent_warning,
            metric_messages: p.accent_primary,
            metric_agents: p.accent_secondary,
            metric_ack_ok: p.accent_success,
            metric_ack_bad: p.accent_error,

            agent_palette: [
                p.accent_slots[0],
                p.accent_slots[1],
                p.accent_slots[2],
                p.accent_slots[3],
                p.accent_slots[4],
                p.accent_slots[5],
                p.accent_slots[6],
                p.accent_slots[7],
            ],

            contact_approved: p.accent_success,
            contact_pending: p.accent_warning,
            contact_blocked: p.accent_error,

            activity_active: p.accent_success,
            activity_idle: p.accent_warning,
            activity_stale: p.fg_disabled,

            text_muted: p.fg_muted,
            text_primary: p.fg_primary,
            text_secondary: p.fg_secondary,
            text_disabled: p.fg_disabled,
            bg_deep: p.bg_deep,
            bg_surface: p.bg_surface,
            bg_overlay: p.bg_overlay,

            toast_error: p.accent_error,
            toast_warning: p.accent_warning,
            toast_info: p.accent_info,
            toast_success: p.accent_success,
            toast_focus: p.accent_info,

            json_key: p.syntax_keyword,
            json_string: p.syntax_string,
            json_number: p.syntax_number,
            json_literal: p.syntax_type,
            json_punctuation: p.fg_muted,
        }
    }

    /// Resolve a palette from the currently active ftui theme.
    #[must_use]
    pub fn current() -> Self {
        if CUSTOM_THEME_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
            return Self::frankenstein();
        }
        Self::for_theme(theme::current_theme())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Style helpers
// ──────────────────────────────────────────────────────────────────────

/// Style for a `MailEventKind` badge / icon.
#[must_use]
pub fn style_for_event_kind(kind: MailEventKind) -> Style {
    let p = theme::current_palette();
    let fg = match kind {
        MailEventKind::ToolCallStart | MailEventKind::ToolCallEnd => p.accent_primary,
        MailEventKind::MessageSent | MailEventKind::MessageReceived => p.accent_info,
        MailEventKind::ReservationGranted | MailEventKind::ReservationReleased => {
            p.accent_secondary
        }
        MailEventKind::AgentRegistered | MailEventKind::ServerStarted => p.accent_success,
        MailEventKind::HttpRequest => p.fg_secondary,
        MailEventKind::HealthPulse => p.fg_muted,
        MailEventKind::ServerShutdown => p.accent_error,
    };
    Style::default().fg(fg)
}

/// Style for an `EventSeverity` badge. Delegates to the severity's own
/// styling but remains available as a theme-integrated entry point.
#[must_use]
pub fn style_for_severity(severity: EventSeverity) -> Style {
    severity.style()
}

/// Style for an HTTP status code.
#[must_use]
pub fn style_for_status(status: u16) -> Style {
    let p = theme::current_palette();
    let fg = match status {
        200..=299 => p.accent_success,
        300..=399 => p.accent_info,
        400..=499 => p.accent_warning,
        _ => p.accent_error,
    };
    Style::default().fg(fg)
}

/// Style for a latency value in milliseconds (green → yellow → red).
#[must_use]
pub fn style_for_latency(ms: u64) -> Style {
    let p = theme::current_palette();
    let fg = if ms < 50 {
        p.accent_success
    } else if ms < 200 {
        p.accent_warning
    } else {
        p.accent_error
    };
    Style::default().fg(fg)
}

/// Style for an agent based on time since last activity.
#[must_use]
pub fn style_for_agent_recency(last_active_secs_ago: u64) -> Style {
    let p = theme::current_palette();
    let fg = if last_active_secs_ago < 60 {
        p.accent_success // active within last minute
    } else if last_active_secs_ago < 600 {
        p.accent_warning // active within last 10 min
    } else {
        p.accent_error // stale
    };
    Style::default().fg(fg)
}

/// Style for a TTL countdown (green → yellow → red → flash).
#[must_use]
pub fn style_for_ttl(remaining_secs: u64) -> Style {
    let p = theme::current_palette();
    let fg = if remaining_secs > 600 {
        p.accent_success
    } else if remaining_secs > 60 {
        p.accent_warning
    } else {
        p.accent_error
    };
    if remaining_secs <= 30 {
        Style::default().fg(fg).bold()
    } else {
        Style::default().fg(fg)
    }
}

/// Cycle to the next theme and return its display name.
///
/// This is the canonical way to switch themes from a keybinding or
/// palette action. It calls `ftui_extras::theme::cycle_theme()`.
#[must_use]
pub fn cycle_and_get_name() -> &'static str {
    // If currently override, disable it and go to first ftui theme.
    if CUSTOM_THEME_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        CUSTOM_THEME_OVERRIDE.store(false, std::sync::atomic::Ordering::Relaxed);
        theme::set_theme(ThemeId::CyberpunkAurora);
        return theme::current_theme_name();
    }

    // Cycle ftui themes
    theme::cycle_theme();

    // If cycled back to start (default), enable override
    if theme::current_theme() == ThemeId::CyberpunkAurora {
        CUSTOM_THEME_OVERRIDE.store(true, std::sync::atomic::Ordering::Relaxed);
        return "Frankenstein";
    }

    theme::current_theme_name()
}

/// Get the current theme display name.
#[must_use]
pub fn current_theme_name() -> &'static str {
    if CUSTOM_THEME_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        return "Frankenstein";
    }
    theme::current_theme_name()
}

/// Return the canonical env value for a [`ThemeId`].
#[must_use]
pub const fn theme_id_env_value(id: ThemeId) -> &'static str {
    match id {
        ThemeId::CyberpunkAurora => "cyberpunk_aurora",
        ThemeId::Darcula => "darcula",
        ThemeId::LumenLight => "lumen_light",
        ThemeId::NordicFrost => "nordic_frost",
        ThemeId::HighContrast => "high_contrast",
    }
}

/// Get the currently active theme ID.
///
/// The "Frankenstein" override is an in-process style override; for
/// persistence and config interop we map it to `CyberpunkAurora`.
#[must_use]
pub fn current_theme_id() -> ThemeId {
    if CUSTOM_THEME_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        return ThemeId::CyberpunkAurora;
    }
    theme::current_theme()
}

/// Get the currently active theme as a canonical env value.
#[must_use]
pub fn current_theme_env_value() -> &'static str {
    theme_id_env_value(current_theme_id())
}

/// Set the active theme directly and return the display name.
#[must_use]
pub fn set_theme_and_get_name(id: ThemeId) -> &'static str {
    CUSTOM_THEME_OVERRIDE.store(false, std::sync::atomic::Ordering::Relaxed);
    theme::set_theme(id);
    theme::current_theme_name()
}

// ──────────────────────────────────────────────────────────────────────
// Markdown Theme Integration
// ──────────────────────────────────────────────────────────────────────

/// Create a [`MarkdownTheme`] that matches the current TUI theme palette.
///
/// This ensures markdown-rendered message bodies use colors consistent
/// with the rest of the TUI, including headings, code blocks, links,
/// task lists, and admonitions.
#[must_use]
pub fn markdown_theme() -> MarkdownTheme {
    let p = theme::current_palette();

    // Build a table theme matching the current palette
    let border = Style::default().fg(p.fg_muted);
    let header = Style::default().fg(p.fg_primary).bg(p.bg_surface).bold();
    let row = Style::default().fg(p.fg_secondary);
    let row_alt = Style::default().fg(p.fg_secondary).bg(p.bg_surface);
    let divider = Style::default().fg(p.fg_muted);

    let table_theme = TableTheme {
        border,
        header,
        row,
        row_alt,
        row_selected: Style::default().fg(p.fg_primary).bg(p.bg_highlight).bold(),
        row_hover: Style::default().fg(p.fg_primary).bg(p.bg_overlay),
        divider,
        padding: 1,
        column_gap: 1,
        row_height: 1,
        effects: Vec::new(),
        preset_id: None,
    };

    MarkdownTheme {
        // Headings: bright to muted gradient using palette colors
        h1: Style::default().fg(p.fg_primary).bold(),
        h2: Style::default().fg(p.accent_primary).bold(),
        h3: Style::default().fg(p.accent_info).bold(),
        h4: Style::default().fg(p.fg_secondary).bold(),
        h5: Style::default().fg(p.fg_muted).bold(),
        h6: Style::default().fg(p.fg_muted),

        // Code: use syntax highlighting colors
        code_inline: Style::default().fg(p.syntax_string),
        code_block: Style::default().fg(p.fg_secondary),

        // Text formatting
        blockquote: Style::default().fg(p.fg_muted).italic(),
        link: Style::default().fg(p.accent_link).underline(),
        emphasis: Style::default().italic(),
        strong: Style::default().bold(),
        strikethrough: Style::default().strikethrough(),

        // Lists
        list_bullet: Style::default().fg(p.accent_secondary),
        horizontal_rule: Style::default().fg(p.fg_muted).dim(),

        // Tables
        table_theme,

        // Task lists
        task_done: Style::default().fg(p.accent_success),
        task_todo: Style::default().fg(p.accent_info),

        // Math
        math_inline: Style::default().fg(p.syntax_number).italic(),
        math_block: Style::default().fg(p.syntax_number).bold(),

        // Footnotes
        footnote_ref: Style::default().fg(p.fg_muted).dim(),
        footnote_def: Style::default().fg(p.fg_muted),

        // Admonitions (GitHub alerts) - semantic colors
        admonition_note: Style::default().fg(p.accent_info).bold(),
        admonition_tip: Style::default().fg(p.accent_success).bold(),
        admonition_important: Style::default().fg(p.accent_primary).bold(),
        admonition_warning: Style::default().fg(p.accent_warning).bold(),
        admonition_caution: Style::default().fg(p.accent_error).bold(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Color utilities
// ──────────────────────────────────────────────────────────────────────

/// Linearly interpolate between two colors.
///
/// `t` is clamped to `[0.0, 1.0]`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]
pub fn lerp_color(a: PackedRgba, b: PackedRgba, t: f32) -> PackedRgba {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    let r = f32::from(a.r()).mul_add(inv, f32::from(b.r()) * t) as u8;
    let g = f32::from(a.g()).mul_add(inv, f32::from(b.g()) * t) as u8;
    let bl = f32::from(a.b()).mul_add(inv, f32::from(b.b()) * t) as u8;
    PackedRgba::rgb(r, g, bl)
}

// ──────────────────────────────────────────────────────────────────────
// Focus-aware panel helpers
// ──────────────────────────────────────────────────────────────────────

/// Return the border color for a panel based on focus state.
#[must_use]
pub const fn focus_border_color(tp: &TuiThemePalette, focused: bool) -> PackedRgba {
    if focused {
        tp.panel_border_focused
    } else {
        tp.panel_border_dim
    }
}

// ──────────────────────────────────────────────────────────────────────
// Selection indicator helpers
// ──────────────────────────────────────────────────────────────────────

/// Prefix string for a selected list item.
pub const SELECTION_PREFIX: &str = "▶ ";
/// Prefix string for an unselected list item (same width).
pub const SELECTION_PREFIX_EMPTY: &str = "  ";

// ──────────────────────────────────────────────────────────────────────
// Semantic typography hierarchy
// ──────────────────────────────────────────────────────────────────────
//
// Six strata of visual importance, from highest to lowest:
//
//   1. **Title**   — Screen/section headings. Bold + primary FG.
//   2. **Section** — Sub-section labels.  Bold + secondary FG.
//   3. **Primary** — Main content text.  Primary FG, normal weight.
//   4. **Meta**    — Supporting metadata (timestamps, counts).  Muted FG.
//   5. **Hint**    — Inline tips, shortcut hints.  Muted FG, dim.
//   6. **Muted**   — Disabled, de-emphasized.  Disabled FG, dim.
//
// Usage: `let s = text_title(&tp);` then apply via `.fg(s.fg).bold()`.

/// Title-level text: screen headings, dialog titles.
#[must_use]
pub fn text_title(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_primary).bold()
}

/// Section-level text: panel headings, group labels.
#[must_use]
pub fn text_section(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_secondary).bold()
}

/// Primary body text: main content, list items.
#[must_use]
pub fn text_primary(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_primary)
}

/// Metadata text: timestamps, IDs, counts, labels.
#[must_use]
pub fn text_meta(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_muted)
}

/// Hint text: inline tips, keyboard shortcut hints.
#[must_use]
pub fn text_hint(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_muted).dim()
}

/// Muted/disabled text: unavailable items, placeholders.
#[must_use]
pub fn text_disabled(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_disabled).dim()
}

// ──────────────────────────────────────────────────────────────────────
// Semantic state style helpers
// ──────────────────────────────────────────────────────────────────────
//
// Consistent state-based styles for actions, severity indicators, and
// status badges.  Use these instead of inline `tp.severity_*` access.

/// Accent/action text: primary CTA, active facet, selected action key.
#[must_use]
pub fn text_accent(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.status_accent).bold()
}

/// Error state text: failures, critical alerts.
#[must_use]
pub fn text_error(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.severity_error).bold()
}

/// Success state text: healthy checks, completed items.
#[must_use]
pub fn text_success(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.severity_ok)
}

/// Warning state text: degraded states, elevated thresholds.
#[must_use]
pub fn text_warning(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.severity_warn).bold()
}

/// Critical state text: highest severity, immediate attention.
#[must_use]
pub fn text_critical(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.severity_critical).bold()
}

/// Facet label text: search facet labels, filter category headings.
#[must_use]
pub fn text_facet_label(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.text_muted)
}

/// Facet active text: selected/active facet value.
#[must_use]
pub fn text_facet_active(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.status_accent)
}

/// Action key hint: keyboard shortcut letters in help/status bars.
#[must_use]
pub fn text_action_key(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.severity_ok)
}

/// Style for an [`mcp_agent_mail_core::AnomalySeverity`] level.
///
/// Used by the analytics screen and any future anomaly/alert surfaces.
#[must_use]
pub fn style_for_anomaly_severity(
    tp: &TuiThemePalette,
    severity: mcp_agent_mail_core::AnomalySeverity,
) -> Style {
    use mcp_agent_mail_core::AnomalySeverity;
    match severity {
        AnomalySeverity::Critical => Style::default().fg(tp.severity_critical).bold(),
        AnomalySeverity::High => Style::default().fg(tp.severity_warn).bold(),
        AnomalySeverity::Medium => Style::default().fg(tp.severity_warn),
        AnomalySeverity::Low => Style::default().fg(tp.severity_ok),
    }
}

// ──────────────────────────────────────────────────────────────────────
// JSON token style helpers
// ──────────────────────────────────────────────────────────────────────

/// Style for a JSON object key (e.g. `"name":`).
#[must_use]
pub fn style_json_key(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.json_key)
}

/// Style for a JSON string value.
#[must_use]
pub fn style_json_string(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.json_string)
}

/// Style for a JSON numeric value.
#[must_use]
pub fn style_json_number(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.json_number)
}

/// Style for a JSON boolean or null literal.
#[must_use]
pub fn style_json_literal(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.json_literal)
}

/// Style for JSON punctuation (`{`, `}`, `[`, `]`, `:`, `,`).
#[must_use]
pub fn style_json_punctuation(tp: &TuiThemePalette) -> Style {
    Style::default().fg(tp.json_punctuation)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_extras::theme::ScopedThemeLock;

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

    #[test]
    fn theme_palettes_meet_min_contrast_thresholds() {
        // Terminal UIs can tolerate slightly lower contrast than strict WCAG AA in practice,
        // but we still enforce a floor to avoid unreadable themes.
        const MIN_TEXT: f64 = 3.0;
        const MIN_ACCENT: f64 = 2.2;

        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);

            let tab_active = contrast_ratio(p.tab_active_fg, p.tab_active_bg);
            let tab_inactive = contrast_ratio(p.tab_inactive_fg, p.tab_inactive_bg);
            let status = contrast_ratio(p.status_fg, p.status_bg);
            let help = contrast_ratio(p.help_fg, p.help_bg);
            let key_hint = contrast_ratio(p.tab_key_fg, p.status_bg);

            // These show up in E2E runs via `cargo test ... -- --nocapture`.
            eprintln!(
                "theme={id:?} tab_active={tab_active:.2} tab_inactive={tab_inactive:.2} status={status:.2} help={help:.2} key_hint={key_hint:.2}"
            );

            assert!(
                tab_active >= MIN_TEXT,
                "theme {id:?}: tab_active contrast {tab_active:.2} < {MIN_TEXT:.1}"
            );
            assert!(
                tab_inactive >= MIN_TEXT,
                "theme {id:?}: tab_inactive contrast {tab_inactive:.2} < {MIN_TEXT:.1}"
            );
            assert!(
                status >= MIN_TEXT,
                "theme {id:?}: status contrast {status:.2} < {MIN_TEXT:.1}"
            );
            assert!(
                help >= MIN_TEXT,
                "theme {id:?}: help contrast {help:.2} < {MIN_TEXT:.1}"
            );
            assert!(
                key_hint >= MIN_ACCENT,
                "theme {id:?}: key_hint contrast {key_hint:.2} < {MIN_ACCENT:.1}"
            );
        }
    }

    #[test]
    fn all_themes_produce_valid_palette() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            // Foreground and accent fields should have visible (non-zero RGB) colors.
            // Background fields (tab_active_bg, tab_inactive_bg, status_bg, help_bg,
            // table_row_alt_bg) are excluded because black (0,0,0) is valid for
            // dark/high-contrast themes.
            let fg_colors = [
                ("tab_active_fg", p.tab_active_fg),
                ("tab_inactive_fg", p.tab_inactive_fg),
                ("tab_key_fg", p.tab_key_fg),
                ("status_fg", p.status_fg),
                ("status_accent", p.status_accent),
                ("status_good", p.status_good),
                ("status_warn", p.status_warn),
                ("help_fg", p.help_fg),
                ("help_key_fg", p.help_key_fg),
                ("help_border_fg", p.help_border_fg),
                ("help_category_fg", p.help_category_fg),
                ("sparkline_lo", p.sparkline_lo),
                ("sparkline_hi", p.sparkline_hi),
                ("table_header_fg", p.table_header_fg),
            ];
            for (name, c) in &fg_colors {
                assert!(
                    c.r() > 0 || c.g() > 0 || c.b() > 0,
                    "theme {id:?} {name} is invisible"
                );
            }
        }
    }

    #[test]
    fn current_palette_matches_active_theme() {
        let _guard = ScopedThemeLock::new(ThemeId::NordicFrost);
        let current = TuiThemePalette::current();
        let explicit = TuiThemePalette::for_theme(ThemeId::NordicFrost);
        assert_eq!(current.tab_key_fg, explicit.tab_key_fg);
        assert_eq!(current.status_good, explicit.status_good);
    }

    #[test]
    fn different_themes_produce_different_palettes() {
        let cyber = {
            let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
            TuiThemePalette::for_theme(ThemeId::CyberpunkAurora)
        };
        let darcula = {
            let _guard = ScopedThemeLock::new(ThemeId::Darcula);
            TuiThemePalette::for_theme(ThemeId::Darcula)
        };
        // At least the key accent should differ
        assert_ne!(
            cyber.tab_key_fg, darcula.tab_key_fg,
            "cyberpunk and darcula tab_key_fg should differ"
        );
    }

    #[test]
    fn style_for_event_kind_all_variants() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let kinds = [
            MailEventKind::ToolCallStart,
            MailEventKind::ToolCallEnd,
            MailEventKind::MessageSent,
            MailEventKind::MessageReceived,
            MailEventKind::ReservationGranted,
            MailEventKind::ReservationReleased,
            MailEventKind::AgentRegistered,
            MailEventKind::HttpRequest,
            MailEventKind::HealthPulse,
            MailEventKind::ServerStarted,
            MailEventKind::ServerShutdown,
        ];
        for kind in kinds {
            let _style = style_for_event_kind(kind);
        }
    }

    #[test]
    fn style_for_status_code_categories() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        // Just ensure no panics and styles are created
        let _s200 = style_for_status(200);
        let _s301 = style_for_status(301);
        let _s404 = style_for_status(404);
        let _s500 = style_for_status(500);
    }

    #[test]
    fn style_for_latency_gradient() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let _fast = style_for_latency(10);
        let _medium = style_for_latency(100);
        let _slow = style_for_latency(500);
    }

    #[test]
    fn style_for_agent_recency_gradient() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let _active = style_for_agent_recency(30);
        let _recent = style_for_agent_recency(300);
        let _stale = style_for_agent_recency(3600);
    }

    #[test]
    fn style_for_ttl_gradient_and_flash() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let _long = style_for_ttl(7200);
        let _medium = style_for_ttl(300);
        let _short = style_for_ttl(45);
        let _flash = style_for_ttl(15);
    }

    #[test]
    fn cycle_returns_new_name() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let name = cycle_and_get_name();
        assert!(!name.is_empty());
        // After cycling from CyberpunkAurora, we should get a different theme
        assert_ne!(name, "Cyberpunk Aurora");
    }

    #[test]
    fn set_theme_and_get_name_sets_requested_theme() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let name = set_theme_and_get_name(ThemeId::Darcula);
        assert_eq!(name, "Darcula");
        assert_eq!(current_theme_id(), ThemeId::Darcula);
    }

    #[test]
    fn current_theme_env_value_tracks_active_theme() {
        let _guard = ScopedThemeLock::new(ThemeId::NordicFrost);
        assert_eq!(current_theme_env_value(), "nordic_frost");
        assert_eq!(theme_id_env_value(ThemeId::HighContrast), "high_contrast");
    }

    #[test]
    fn markdown_theme_respects_current_theme() {
        // Test that markdown_theme() produces different styles for different themes
        let cyber = {
            let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
            markdown_theme()
        };
        let darcula = {
            let _guard = ScopedThemeLock::new(ThemeId::Darcula);
            markdown_theme()
        };
        // The h1 style should differ between themes (both use palette fg_primary)
        // Just verify that the function runs without panic and returns something valid
        assert!(cyber.h1.fg.is_some());
        assert!(darcula.h1.fg.is_some());
        // Link style should use palette accent_link (verify it's set)
        assert!(cyber.link.fg.is_some());
        // Table theme should have visible border style
        assert!(cyber.table_theme.border.fg.is_some());
    }

    #[test]
    fn markdown_theme_has_complete_styles() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let theme = markdown_theme();

        // Verify all heading levels have foreground colors
        assert!(theme.h1.fg.is_some(), "h1 should have fg color");
        assert!(theme.h2.fg.is_some(), "h2 should have fg color");
        assert!(theme.h3.fg.is_some(), "h3 should have fg color");
        assert!(theme.h4.fg.is_some(), "h4 should have fg color");
        assert!(theme.h5.fg.is_some(), "h5 should have fg color");
        assert!(theme.h6.fg.is_some(), "h6 should have fg color");

        // Verify code styles
        assert!(theme.code_inline.fg.is_some(), "code_inline should have fg");
        assert!(theme.code_block.fg.is_some(), "code_block should have fg");

        // Verify semantic styles
        assert!(theme.link.fg.is_some(), "link should have fg");
        assert!(theme.task_done.fg.is_some(), "task_done should have fg");
        assert!(theme.task_todo.fg.is_some(), "task_todo should have fg");

        // Verify admonition styles
        assert!(
            theme.admonition_note.fg.is_some(),
            "admonition_note should have fg"
        );
        assert!(
            theme.admonition_warning.fg.is_some(),
            "admonition_warning should have fg"
        );
        assert!(
            theme.admonition_caution.fg.is_some(),
            "admonition_caution should have fg"
        );
    }

    // ── Semantic typography hierarchy tests ──────────────────────

    #[test]
    fn typography_hierarchy_has_distinct_strata() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let tp = TuiThemePalette::current();
        let title = text_title(&tp);
        let section = text_section(&tp);
        let primary = text_primary(&tp);
        let meta = text_meta(&tp);
        let hint = text_hint(&tp);
        let disabled = text_disabled(&tp);

        // Title and section should have fg set.
        assert!(title.fg.is_some(), "title needs fg");
        assert!(section.fg.is_some(), "section needs fg");
        assert!(primary.fg.is_some(), "primary needs fg");
        assert!(meta.fg.is_some(), "meta needs fg");
        assert!(hint.fg.is_some(), "hint needs fg");
        assert!(disabled.fg.is_some(), "disabled needs fg");

        // Title should be bold.
        use ftui::style::StyleFlags;
        let has = |s: &Style, f: StyleFlags| s.attrs.map_or(false, |a| a.contains(f));
        assert!(has(&title, StyleFlags::BOLD), "title must be bold");
        assert!(has(&section, StyleFlags::BOLD), "section must be bold");

        // Hint and disabled should be dim.
        assert!(has(&hint, StyleFlags::DIM), "hint must be dim");
        assert!(has(&disabled, StyleFlags::DIM), "disabled must be dim");

        // Primary should NOT be bold.
        assert!(!has(&primary, StyleFlags::BOLD), "primary should not be bold");
    }

    #[test]
    fn typography_hierarchy_consistent_across_themes() {
        for &theme_id in &[
            ThemeId::CyberpunkAurora,
            ThemeId::Darcula,
            ThemeId::NordicFrost,
            ThemeId::HighContrast,
        ] {
            let _guard = ScopedThemeLock::new(theme_id);
            let tp = TuiThemePalette::current();

            // Every theme must produce valid (non-zero fg) styles.
            let styles = [
                ("title", text_title(&tp)),
                ("section", text_section(&tp)),
                ("primary", text_primary(&tp)),
                ("meta", text_meta(&tp)),
                ("hint", text_hint(&tp)),
                ("disabled", text_disabled(&tp)),
            ];
            for (name, style) in &styles {
                assert!(
                    style.fg.is_some(),
                    "{name} missing fg in theme {theme_id:?}"
                );
            }
        }
    }

    // ── Semantic state style tests ──────────────────────────────

    #[test]
    fn semantic_state_helpers_produce_valid_styles() {
        use ftui::style::StyleFlags;
        let has = |s: &Style, f: StyleFlags| s.attrs.map_or(false, |a| a.contains(f));

        for &theme_id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(theme_id);
            let tp = TuiThemePalette::current();

            let accent = text_accent(&tp);
            let error = text_error(&tp);
            let success = text_success(&tp);
            let warning = text_warning(&tp);
            let critical = text_critical(&tp);
            let facet_label = text_facet_label(&tp);
            let facet_active = text_facet_active(&tp);
            let action_key = text_action_key(&tp);

            // All should have fg set.
            for (name, s) in &[
                ("accent", &accent),
                ("error", &error),
                ("success", &success),
                ("warning", &warning),
                ("critical", &critical),
                ("facet_label", &facet_label),
                ("facet_active", &facet_active),
                ("action_key", &action_key),
            ] {
                assert!(
                    s.fg.is_some(),
                    "{name} missing fg in theme {theme_id:?}"
                );
            }

            // Bold expectations.
            assert!(has(&accent, StyleFlags::BOLD), "accent must be bold in {theme_id:?}");
            assert!(has(&error, StyleFlags::BOLD), "error must be bold in {theme_id:?}");
            assert!(has(&warning, StyleFlags::BOLD), "warning must be bold in {theme_id:?}");
            assert!(has(&critical, StyleFlags::BOLD), "critical must be bold in {theme_id:?}");

            // Success is intentionally NOT bold (lower visual weight).
            assert!(!has(&success, StyleFlags::BOLD), "success should not be bold in {theme_id:?}");
        }
    }

    #[test]
    fn anomaly_severity_style_maps_correctly() {
        use mcp_agent_mail_core::AnomalySeverity;
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let tp = TuiThemePalette::current();

        let crit = style_for_anomaly_severity(&tp, AnomalySeverity::Critical);
        let high = style_for_anomaly_severity(&tp, AnomalySeverity::High);
        let med = style_for_anomaly_severity(&tp, AnomalySeverity::Medium);
        let low = style_for_anomaly_severity(&tp, AnomalySeverity::Low);

        // All should produce distinct foreground colors.
        assert!(crit.fg.is_some());
        assert!(high.fg.is_some());
        assert!(med.fg.is_some());
        assert!(low.fg.is_some());

        // Critical and high should use different base colors.
        assert_ne!(crit.fg, low.fg, "critical and low should differ");
    }

    // ──────────────────────────────────────────────────────────────────
    // Semantic color hierarchy validation (br-1xt0m.1.13.9)
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn semantic_color_hierarchy_warn_distinct_from_good() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            assert_ne!(
                p.status_good, p.status_warn,
                "theme {id:?}: status_good and status_warn must differ"
            );
        }
    }

    #[test]
    fn semantic_color_hierarchy_accent_distinct_from_fg() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            assert_ne!(
                p.status_accent, p.status_fg,
                "theme {id:?}: status_accent and status_fg must differ"
            );
        }
    }

    #[test]
    fn semantic_color_hierarchy_sparkline_lo_hi_distinct() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            assert_ne!(
                p.sparkline_lo, p.sparkline_hi,
                "theme {id:?}: sparkline_lo and sparkline_hi must differ"
            );
        }
    }

    #[test]
    fn semantic_color_hierarchy_active_tab_readable() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            // Active tab FG should differ from BG to be readable.
            assert_ne!(
                p.tab_active_fg, p.tab_active_bg,
                "theme {id:?}: active tab FG and BG must differ"
            );
        }
    }

    #[test]
    fn semantic_color_hierarchy_help_key_distinct_from_help_fg() {
        for &id in &ThemeId::ALL {
            let _guard = ScopedThemeLock::new(id);
            let p = TuiThemePalette::for_theme(id);
            assert_ne!(
                p.help_key_fg, p.help_fg,
                "theme {id:?}: help_key_fg and help_fg must differ for visual hierarchy"
            );
        }
    }
}
