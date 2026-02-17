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

/// Active named theme index (`0..NAMED_THEME_COUNT`). Used by `cycle_named_theme`.
static ACTIVE_NAMED_THEME_INDEX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ──────────────────────────────────────────────────────────────────────
// Named theme registry
// ──────────────────────────────────────────────────────────────────────

/// Named themes in cycling order: (`config_name`, `display_name`).
///
/// The `config_name` matches values accepted by `AM_TUI_THEME` / `from_config_name`.
/// The `display_name` is shown in the status line and command palette.
pub const NAMED_THEMES: &[(&str, &str)] = &[
    ("default", "Frankenstein"),
    ("solarized", "Solarized Dark"),
    ("dracula", "Dracula"),
    ("nord", "Nord"),
    ("gruvbox", "Gruvbox Dark"),
];

/// Number of built-in named themes.
pub const NAMED_THEME_COUNT: usize = 5;

/// Get the config name for a theme index (wraps modulo `NAMED_THEME_COUNT`).
#[must_use]
pub fn named_theme_config_name(index: usize) -> &'static str {
    NAMED_THEMES[index % NAMED_THEME_COUNT].0
}

/// Get the display name for a theme index (wraps modulo `NAMED_THEME_COUNT`).
#[must_use]
pub fn named_theme_display_name(index: usize) -> &'static str {
    NAMED_THEMES[index % NAMED_THEME_COUNT].1
}

/// Initialize the active named theme from a config name (call once at startup).
///
/// Sets the internal index so that subsequent `cycle_named_theme` calls
/// cycle from the correct starting position.
pub fn init_named_theme(config_name: &str) {
    let idx = TuiThemePalette::config_name_to_index(config_name);
    ACTIVE_NAMED_THEME_INDEX.store(idx, std::sync::atomic::Ordering::Relaxed);
}

/// Get the current active named theme index.
#[must_use]
pub fn active_named_theme_index() -> usize {
    ACTIVE_NAMED_THEME_INDEX.load(std::sync::atomic::Ordering::Relaxed)
}

/// Get the current active named theme's config name.
#[must_use]
pub fn active_named_theme_config_name() -> &'static str {
    named_theme_config_name(active_named_theme_index())
}

/// Get the current active named theme's display name.
#[must_use]
pub fn active_named_theme_display() -> &'static str {
    named_theme_display_name(active_named_theme_index())
}

/// Get the current active named theme palette.
#[must_use]
pub fn active_named_palette() -> TuiThemePalette {
    TuiThemePalette::from_index(active_named_theme_index())
}

/// Cycle to the next named theme and return
/// (`config_name`, `display_name`, `palette`).
///
/// Increments the active index modulo [`NAMED_THEME_COUNT`].
/// Callers should persist the returned `config_name` to the envfile.
pub fn cycle_named_theme() -> (&'static str, &'static str, TuiThemePalette) {
    let old = ACTIVE_NAMED_THEME_INDEX.load(std::sync::atomic::Ordering::Relaxed);
    let new = (old + 1) % NAMED_THEME_COUNT;
    ACTIVE_NAMED_THEME_INDEX.store(new, std::sync::atomic::Ordering::Relaxed);
    let (cfg, display) = NAMED_THEMES[new];
    (cfg, display, TuiThemePalette::from_config_name(cfg))
}

/// Set the active named theme by index and return
/// (`config_name`, `display_name`, `palette`).
///
/// Used by command palette to select a theme directly.
pub fn set_named_theme(index: usize) -> (&'static str, &'static str, TuiThemePalette) {
    let idx = index % NAMED_THEME_COUNT;
    ACTIVE_NAMED_THEME_INDEX.store(idx, std::sync::atomic::Ordering::Relaxed);
    let (cfg, display) = NAMED_THEMES[idx];
    (cfg, display, TuiThemePalette::from_config_name(cfg))
}

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
    pub metric_reservations: PackedRgba,
    pub metric_projects: PackedRgba,

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
    #[allow(clippy::too_many_lines)] // Literal palette table is clearer than splitting into indirections.
    #[must_use]
    pub const fn frankenstein() -> Self {
        // Palette:
        // Deep Abyss BG: 5, 10, 5 (Darker)
        // Electric Neon Green FG: 50, 255, 50 (More vibrant)
        // Hyper Purple Accent: 200, 50, 255 (More vibrant)
        // Stitch Cyan: 50, 200, 200
        // Blood Red: 255, 20, 20

        let bg_deep = PackedRgba::rgb(5, 10, 5);
        let bg_surface = PackedRgba::rgb(15, 25, 15);
        let fg_primary = PackedRgba::rgb(50, 255, 50); // Electric Green
        let fg_muted = PackedRgba::rgb(40, 100, 40);
        let accent = PackedRgba::rgb(200, 50, 255); // Hyper Purple
        let warning = PackedRgba::rgb(255, 220, 0); // High-voltage Yellow
        let _error = PackedRgba::rgb(255, 20, 20); // Blood Red

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

            selection_bg: PackedRgba::rgb(30, 60, 30),
            selection_fg: fg_primary,

            severity_ok: PackedRgba::rgb(50, 255, 50),
            severity_error: PackedRgba::rgb(255, 20, 20),
            severity_warn: warning,
            severity_critical: PackedRgba::rgb(255, 0, 0),

            panel_border: fg_muted,
            panel_border_focused: accent,
            panel_border_dim: PackedRgba::rgb(20, 40, 20),
            panel_bg: bg_deep,
            panel_title_fg: fg_primary,

            selection_indicator: accent,
            list_hover_bg: PackedRgba::rgb(20, 40, 20),

            chart_series: [
                PackedRgba::rgb(50, 255, 50),
                PackedRgba::rgb(50, 200, 255),
                PackedRgba::rgb(255, 220, 0),
                PackedRgba::rgb(255, 50, 150),
                PackedRgba::rgb(200, 50, 255),
                PackedRgba::rgb(255, 100, 0),
            ],
            chart_axis: fg_muted,
            chart_grid: PackedRgba::rgb(20, 40, 20),

            badge_urgent_bg: PackedRgba::rgb(200, 0, 0),
            badge_urgent_fg: PackedRgba::rgb(255, 255, 255),
            badge_info_bg: PackedRgba::rgb(20, 40, 60),
            badge_info_fg: PackedRgba::rgb(100, 200, 255),

            ttl_healthy: PackedRgba::rgb(50, 255, 50),
            ttl_warning: warning,
            ttl_danger: PackedRgba::rgb(255, 50, 0),
            ttl_expired: PackedRgba::rgb(100, 50, 50),

            metric_uptime: PackedRgba::rgb(50, 255, 50),
            metric_requests: PackedRgba::rgb(50, 200, 255),
            metric_latency: PackedRgba::rgb(255, 220, 0),
            metric_messages: PackedRgba::rgb(200, 50, 255),
            metric_agents: PackedRgba::rgb(255, 50, 150),
            metric_ack_ok: PackedRgba::rgb(50, 255, 50),
            metric_ack_bad: PackedRgba::rgb(255, 20, 20),
            metric_reservations: warning,
            metric_projects: PackedRgba::rgb(50, 200, 200),

            agent_palette: [
                PackedRgba::rgb(50, 200, 255),
                PackedRgba::rgb(50, 255, 50),
                PackedRgba::rgb(255, 220, 0),
                PackedRgba::rgb(255, 50, 150),
                PackedRgba::rgb(200, 50, 255),
                PackedRgba::rgb(50, 200, 200),
                PackedRgba::rgb(255, 150, 50),
                PackedRgba::rgb(150, 150, 150),
            ],

            contact_approved: PackedRgba::rgb(50, 255, 50),
            contact_pending: warning,
            contact_blocked: PackedRgba::rgb(255, 20, 20),

            activity_active: PackedRgba::rgb(150, 255, 150),
            activity_idle: PackedRgba::rgb(100, 200, 100),
            activity_stale: PackedRgba::rgb(60, 80, 60),

            text_muted: fg_muted,
            text_primary: fg_primary,
            text_secondary: PackedRgba::rgb(100, 200, 100),
            text_disabled: PackedRgba::rgb(40, 60, 40),
            bg_deep,
            bg_surface,
            bg_overlay: PackedRgba::rgb(25, 45, 25),

            toast_error: PackedRgba::rgb(255, 50, 50),
            toast_warning: warning,
            toast_info: PackedRgba::rgb(50, 200, 255),
            toast_success: PackedRgba::rgb(50, 255, 50),
            toast_focus: accent,

            json_key: accent,
            json_string: PackedRgba::rgb(50, 255, 50),
            json_number: warning,
            json_literal: PackedRgba::rgb(50, 200, 255),
            json_punctuation: fg_muted,
        }
    }

    /// Solarized Dark theme based on Ethan Schoonover's official palette.
    ///
    /// <https://ethanschoonover.com/solarized/>
    #[must_use]
    pub const fn solarized_dark() -> Self {
        // Solarized base tones
        let base03 = PackedRgba::rgb(0, 43, 54); // #002b36 — dark bg
        let base02 = PackedRgba::rgb(7, 54, 66); // #073642 — bg highlights
        let base01 = PackedRgba::rgb(88, 110, 117); // #586e75 — comments/secondary
        let base00 = PackedRgba::rgb(101, 123, 131); // #657b83
        let base0 = PackedRgba::rgb(131, 148, 150); // #839496 — body text
        let base_light = PackedRgba::rgb(147, 161, 161); // #93a1a1 — emphasis
        // Solarized accent colors
        let yellow = PackedRgba::rgb(181, 137, 0); // #b58900
        let orange = PackedRgba::rgb(203, 75, 22); // #cb4b16
        let red = PackedRgba::rgb(220, 50, 47); // #dc322f
        let magenta = PackedRgba::rgb(211, 54, 130); // #d33682
        let violet = PackedRgba::rgb(108, 113, 196); // #6c71c4
        let blue = PackedRgba::rgb(38, 139, 210); // #268bd2
        let cyan = PackedRgba::rgb(42, 161, 152); // #2aa198
        let green = PackedRgba::rgb(133, 153, 0); // #859900

        Self {
            tab_active_bg: base02,
            tab_active_fg: base_light,
            tab_inactive_bg: base03,
            tab_inactive_fg: base01,
            tab_key_fg: blue,

            status_bg: base03,
            status_fg: base0,
            status_accent: blue,
            status_good: green,
            status_warn: yellow,

            help_bg: base03,
            help_fg: base0,
            help_key_fg: blue,
            help_border_fg: base01,
            help_category_fg: cyan,

            sparkline_lo: base01,
            sparkline_hi: green,

            table_header_fg: blue,
            table_row_alt_bg: base02,

            selection_bg: PackedRgba::rgb(17, 70, 85),
            selection_fg: base_light,

            severity_ok: green,
            severity_error: red,
            severity_warn: yellow,
            severity_critical: orange,

            panel_border: base01,
            panel_border_focused: blue,
            panel_border_dim: base00,
            panel_bg: base03,
            panel_title_fg: base_light,

            selection_indicator: blue,
            list_hover_bg: PackedRgba::rgb(0, 55, 70),

            chart_series: [green, blue, yellow, red, violet, cyan],
            chart_axis: base01,
            chart_grid: base02,

            badge_urgent_bg: red,
            badge_urgent_fg: base_light,
            badge_info_bg: base02,
            badge_info_fg: cyan,

            ttl_healthy: green,
            ttl_warning: yellow,
            ttl_danger: orange,
            ttl_expired: base01,

            metric_uptime: green,
            metric_requests: blue,
            metric_latency: yellow,
            metric_messages: violet,
            metric_agents: magenta,
            metric_ack_ok: green,
            metric_ack_bad: red,
            metric_reservations: yellow,
            metric_projects: cyan,

            agent_palette: [
                blue, green, yellow, orange, violet, cyan, magenta, base_light,
            ],

            contact_approved: green,
            contact_pending: yellow,
            contact_blocked: red,

            activity_active: green,
            activity_idle: yellow,
            activity_stale: base01,

            text_muted: base01,
            text_primary: base0,
            text_secondary: base_light,
            text_disabled: base00,
            bg_deep: base03,
            bg_surface: base02,
            bg_overlay: PackedRgba::rgb(15, 65, 78),

            toast_error: red,
            toast_warning: orange,
            toast_info: cyan,
            toast_success: green,
            toast_focus: blue,

            json_key: blue,
            json_string: cyan,
            json_number: magenta,
            json_literal: yellow,
            json_punctuation: base01,
        }
    }

    /// Dracula theme based on the official Dracula palette.
    ///
    /// Maps to ftui's `Darcula` `ThemeId` which implements the same palette.
    ///
    /// <https://draculatheme.com/>
    #[must_use]
    pub const fn dracula() -> Self {
        // Dracula base tones
        let bg = PackedRgba::rgb(40, 42, 54); // #282a36
        let current_line = PackedRgba::rgb(68, 71, 90); // #44475a
        let fg = PackedRgba::rgb(248, 248, 242); // #f8f8f2
        let comment = PackedRgba::rgb(98, 114, 164); // #6272a4
        // Dracula accent colors
        let cyan_d = PackedRgba::rgb(139, 233, 253); // #8be9fd
        let green_d = PackedRgba::rgb(80, 250, 123); // #50fa7b
        let orange_d = PackedRgba::rgb(255, 184, 108); // #ffb86c
        let pink = PackedRgba::rgb(255, 121, 198); // #ff79c6
        let purple = PackedRgba::rgb(189, 147, 249); // #bd93f9
        let red_d = PackedRgba::rgb(255, 85, 85); // #ff5555
        let yellow_d = PackedRgba::rgb(241, 250, 140); // #f1fa8c

        let bg_deep = PackedRgba::rgb(30, 31, 41);

        Self {
            tab_active_bg: current_line,
            tab_active_fg: fg,
            tab_inactive_bg: bg,
            tab_inactive_fg: comment,
            tab_key_fg: purple,

            status_bg: bg_deep,
            status_fg: fg,
            status_accent: purple,
            status_good: green_d,
            status_warn: yellow_d,

            help_bg: bg_deep,
            help_fg: fg,
            help_key_fg: purple,
            help_border_fg: comment,
            help_category_fg: cyan_d,

            sparkline_lo: comment,
            sparkline_hi: green_d,

            table_header_fg: purple,
            table_row_alt_bg: current_line,

            selection_bg: PackedRgba::rgb(55, 58, 78),
            selection_fg: fg,

            severity_ok: green_d,
            severity_error: red_d,
            severity_warn: yellow_d,
            severity_critical: PackedRgba::rgb(255, 50, 50),

            panel_border: comment,
            panel_border_focused: purple,
            panel_border_dim: PackedRgba::rgb(55, 58, 78),
            panel_bg: bg_deep,
            panel_title_fg: fg,

            selection_indicator: purple,
            list_hover_bg: PackedRgba::rgb(50, 52, 68),

            chart_series: [green_d, cyan_d, orange_d, pink, purple, yellow_d],
            chart_axis: comment,
            chart_grid: current_line,

            badge_urgent_bg: red_d,
            badge_urgent_fg: fg,
            badge_info_bg: current_line,
            badge_info_fg: cyan_d,

            ttl_healthy: green_d,
            ttl_warning: yellow_d,
            ttl_danger: orange_d,
            ttl_expired: comment,

            metric_uptime: green_d,
            metric_requests: cyan_d,
            metric_latency: orange_d,
            metric_messages: purple,
            metric_agents: pink,
            metric_ack_ok: green_d,
            metric_ack_bad: red_d,
            metric_reservations: yellow_d,
            metric_projects: cyan_d,

            agent_palette: [
                cyan_d, green_d, orange_d, pink, purple, yellow_d, red_d, comment,
            ],

            contact_approved: green_d,
            contact_pending: yellow_d,
            contact_blocked: red_d,

            activity_active: green_d,
            activity_idle: yellow_d,
            activity_stale: comment,

            text_muted: comment,
            text_primary: fg,
            text_secondary: PackedRgba::rgb(220, 220, 210),
            text_disabled: PackedRgba::rgb(70, 72, 90),
            bg_deep,
            bg_surface: bg,
            bg_overlay: current_line,

            toast_error: red_d,
            toast_warning: orange_d,
            toast_info: cyan_d,
            toast_success: green_d,
            toast_focus: purple,

            json_key: purple,
            json_string: yellow_d,
            json_number: cyan_d,
            json_literal: pink,
            json_punctuation: comment,
        }
    }

    #[allow(clippy::similar_names)] // Nord palette uses official numbered tokens from spec.
    /// Nord theme based on Arctic Ice Studio's official palette.
    ///
    /// <https://www.nordtheme.com/>
    #[must_use]
    pub const fn nord() -> Self {
        // Polar Night
        let nord0 = PackedRgba::rgb(46, 52, 64); // #2e3440
        let nord1 = PackedRgba::rgb(59, 66, 82); // #3b4252
        let nord2 = PackedRgba::rgb(67, 76, 94); // #434c5e
        let nord3 = PackedRgba::rgb(76, 86, 106); // #4c566a
        // Snow Storm
        let nord4 = PackedRgba::rgb(216, 222, 233); // #d8dee9
        let nord5 = PackedRgba::rgb(229, 233, 240); // #e5e9f0
        // nord6 (#eceff4) available but unused in dark theme
        // Frost
        let nord7 = PackedRgba::rgb(143, 188, 187); // #8fbcbb
        let nord8 = PackedRgba::rgb(136, 192, 208); // #88c0d0
        let nord9 = PackedRgba::rgb(129, 161, 193); // #81a1c1
        let nord10 = PackedRgba::rgb(94, 129, 172); // #5e81ac
        // Aurora
        let nord11 = PackedRgba::rgb(191, 97, 106); // #bf616a — red
        let nord12 = PackedRgba::rgb(208, 135, 112); // #d08770 — orange
        let nord13 = PackedRgba::rgb(235, 203, 139); // #ebcb8b — yellow
        let nord14 = PackedRgba::rgb(163, 190, 140); // #a3be8c — green
        let nord15 = PackedRgba::rgb(180, 142, 173); // #b48ead — purple

        let bg_deep = PackedRgba::rgb(36, 40, 50);

        Self {
            tab_active_bg: nord1,
            tab_active_fg: nord5,
            tab_inactive_bg: nord0,
            tab_inactive_fg: nord3,
            tab_key_fg: nord8,

            status_bg: bg_deep,
            status_fg: nord4,
            status_accent: nord8,
            status_good: nord14,
            status_warn: nord13,

            help_bg: bg_deep,
            help_fg: nord4,
            help_key_fg: nord8,
            help_border_fg: nord3,
            help_category_fg: nord9,

            sparkline_lo: nord3,
            sparkline_hi: nord14,

            table_header_fg: nord8,
            table_row_alt_bg: nord1,

            selection_bg: nord2,
            selection_fg: nord5,

            severity_ok: nord14,
            severity_error: nord11,
            severity_warn: nord13,
            severity_critical: PackedRgba::rgb(210, 80, 90),

            panel_border: nord3,
            panel_border_focused: nord8,
            panel_border_dim: nord2,
            panel_bg: bg_deep,
            panel_title_fg: nord4,

            selection_indicator: nord8,
            list_hover_bg: PackedRgba::rgb(52, 58, 74),

            chart_series: [nord14, nord8, nord13, nord11, nord15, nord7],
            chart_axis: nord3,
            chart_grid: nord1,

            badge_urgent_bg: nord11,
            badge_urgent_fg: nord5,
            badge_info_bg: nord2,
            badge_info_fg: nord8,

            ttl_healthy: nord14,
            ttl_warning: nord13,
            ttl_danger: nord12,
            ttl_expired: nord3,

            metric_uptime: nord14,
            metric_requests: nord8,
            metric_latency: nord13,
            metric_messages: nord15,
            metric_agents: nord9,
            metric_ack_ok: nord14,
            metric_ack_bad: nord11,
            metric_reservations: nord13,
            metric_projects: nord8,

            agent_palette: [nord8, nord14, nord13, nord12, nord15, nord7, nord9, nord10],

            contact_approved: nord14,
            contact_pending: nord13,
            contact_blocked: nord11,

            activity_active: nord14,
            activity_idle: nord13,
            activity_stale: nord3,

            text_muted: nord3,
            text_primary: nord4,
            text_secondary: nord5,
            text_disabled: nord2,
            bg_deep,
            bg_surface: nord0,
            bg_overlay: nord1,

            toast_error: nord11,
            toast_warning: nord12,
            toast_info: nord8,
            toast_success: nord14,
            toast_focus: nord9,

            json_key: nord8,
            json_string: nord14,
            json_number: nord15,
            json_literal: nord13,
            json_punctuation: nord3,
        }
    }

    /// Gruvbox Dark theme based on morhetz's official palette.
    ///
    /// <https://github.com/morhetz/gruvbox>
    #[must_use]
    pub const fn gruvbox_dark() -> Self {
        // Gruvbox dark backgrounds
        let bg0 = PackedRgba::rgb(40, 40, 40); // #282828
        let bg1 = PackedRgba::rgb(60, 56, 54); // #3c3836
        let bg2 = PackedRgba::rgb(80, 73, 69); // #504945
        let bg3 = PackedRgba::rgb(102, 92, 84); // #665c54
        // Gruvbox foregrounds
        let fg0 = PackedRgba::rgb(235, 219, 178); // #ebdbb2
        let fg2 = PackedRgba::rgb(213, 196, 161); // #d5c4a1
        let fg4 = PackedRgba::rgb(168, 153, 132); // #a89984
        // Gruvbox bright accents
        let red = PackedRgba::rgb(251, 73, 52); // #fb4934
        let green = PackedRgba::rgb(184, 187, 38); // #b8bb26
        let yellow = PackedRgba::rgb(250, 189, 47); // #fabd2f
        let blue = PackedRgba::rgb(131, 165, 152); // #83a598
        let purple = PackedRgba::rgb(211, 134, 155); // #d3869b
        let aqua = PackedRgba::rgb(142, 192, 124); // #8ec07c
        let orange = PackedRgba::rgb(254, 128, 25); // #fe8019

        let bg_deep = PackedRgba::rgb(29, 29, 29);

        Self {
            tab_active_bg: bg1,
            tab_active_fg: fg0,
            tab_inactive_bg: bg0,
            tab_inactive_fg: fg4,
            tab_key_fg: yellow,

            status_bg: bg_deep,
            status_fg: fg0,
            status_accent: yellow,
            status_good: green,
            status_warn: orange,

            help_bg: bg_deep,
            help_fg: fg0,
            help_key_fg: yellow,
            help_border_fg: fg4,
            help_category_fg: aqua,

            sparkline_lo: fg4,
            sparkline_hi: green,

            table_header_fg: yellow,
            table_row_alt_bg: bg1,

            selection_bg: bg2,
            selection_fg: fg0,

            severity_ok: green,
            severity_error: red,
            severity_warn: orange,
            severity_critical: PackedRgba::rgb(255, 50, 40),

            panel_border: fg4,
            panel_border_focused: yellow,
            panel_border_dim: bg3,
            panel_bg: bg_deep,
            panel_title_fg: fg0,

            selection_indicator: yellow,
            list_hover_bg: PackedRgba::rgb(50, 48, 47),

            chart_series: [green, blue, yellow, red, purple, aqua],
            chart_axis: fg4,
            chart_grid: bg1,

            badge_urgent_bg: red,
            badge_urgent_fg: fg0,
            badge_info_bg: bg2,
            badge_info_fg: blue,

            ttl_healthy: green,
            ttl_warning: yellow,
            ttl_danger: orange,
            ttl_expired: bg3,

            metric_uptime: green,
            metric_requests: blue,
            metric_latency: orange,
            metric_messages: purple,
            metric_agents: aqua,
            metric_ack_ok: green,
            metric_ack_bad: red,
            metric_reservations: yellow,
            metric_projects: blue,

            agent_palette: [blue, green, yellow, orange, purple, aqua, red, fg4],

            contact_approved: green,
            contact_pending: yellow,
            contact_blocked: red,

            activity_active: green,
            activity_idle: yellow,
            activity_stale: bg3,

            text_muted: fg4,
            text_primary: fg0,
            text_secondary: fg2,
            text_disabled: bg3,
            bg_deep,
            bg_surface: bg0,
            bg_overlay: bg1,

            toast_error: red,
            toast_warning: orange,
            toast_info: blue,
            toast_success: green,
            toast_focus: aqua,

            json_key: yellow,
            json_string: green,
            json_number: purple,
            json_literal: orange,
            json_punctuation: fg4,
        }
    }

    /// Resolve a named theme from the `AM_TUI_THEME` config value.
    ///
    /// Accepted values: `"default"`, `"solarized"`, `"dracula"`, `"nord"`, `"gruvbox"`.
    /// Unknown values fall back to the default theme.
    #[must_use]
    pub fn from_config_name(name: &str) -> Self {
        match name {
            "solarized" => Self::solarized_dark(),
            "dracula" => Self::dracula(),
            "nord" => Self::nord(),
            "gruvbox" => Self::gruvbox_dark(),
            _ => Self::frankenstein(),
        }
    }

    /// Resolve a palette by its zero-based index in the named theme registry.
    ///
    /// The index wraps modulo [`NAMED_THEME_COUNT`].
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        Self::from_config_name(named_theme_config_name(index))
    }

    /// Convert a config name to its index in the named theme registry.
    ///
    /// Returns `0` (default) for unrecognized names.
    #[must_use]
    pub fn config_name_to_index(name: &str) -> usize {
        NAMED_THEMES
            .iter()
            .position(|(cfg, _)| *cfg == name)
            .unwrap_or(0)
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
            metric_reservations: p.accent_warning,
            metric_projects: p.accent_info,

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

    /// Resolve the palette for the currently active named theme.
    ///
    /// Uses the named theme index set by [`init_named_theme`] or
    /// [`cycle_named_theme`].
    #[must_use]
    pub fn current() -> Self {
        active_named_palette()
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
    fn current_palette_matches_active_named_theme() {
        init_named_theme("nord");
        let current = TuiThemePalette::current();
        let explicit = TuiThemePalette::nord();
        assert_eq!(current.tab_key_fg, explicit.tab_key_fg);
        assert_eq!(current.status_good, explicit.status_good);
        init_named_theme("default");
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
    #[allow(clippy::items_after_statements)]
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
        let has = |s: &Style, f: StyleFlags| s.attrs.is_some_and(|a| a.contains(f));
        assert!(has(&title, StyleFlags::BOLD), "title must be bold");
        assert!(has(&section, StyleFlags::BOLD), "section must be bold");

        // Hint and disabled should be dim.
        assert!(has(&hint, StyleFlags::DIM), "hint must be dim");
        assert!(has(&disabled, StyleFlags::DIM), "disabled must be dim");

        // Primary should NOT be bold.
        assert!(
            !has(&primary, StyleFlags::BOLD),
            "primary should not be bold"
        );
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
        let has = |s: &Style, f: StyleFlags| s.attrs.is_some_and(|a| a.contains(f));

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
                assert!(s.fg.is_some(), "{name} missing fg in theme {theme_id:?}");
            }

            // Bold expectations.
            assert!(
                has(&accent, StyleFlags::BOLD),
                "accent must be bold in {theme_id:?}"
            );
            assert!(
                has(&error, StyleFlags::BOLD),
                "error must be bold in {theme_id:?}"
            );
            assert!(
                has(&warning, StyleFlags::BOLD),
                "warning must be bold in {theme_id:?}"
            );
            assert!(
                has(&critical, StyleFlags::BOLD),
                "critical must be bold in {theme_id:?}"
            );

            // Success is intentionally NOT bold (lower visual weight).
            assert!(
                !has(&success, StyleFlags::BOLD),
                "success should not be bold in {theme_id:?}"
            );
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

    // ── Named theme variant tests (br-2k9ze) ────────────────────

    /// All 5 named theme constructors produce valid (non-zero fg) palettes.
    #[test]
    fn named_themes_produce_valid_palettes() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            // Foreground fields must be visible (non-zero RGB).
            let fg_fields = [
                ("tab_active_fg", p.tab_active_fg),
                ("tab_inactive_fg", p.tab_inactive_fg),
                ("tab_key_fg", p.tab_key_fg),
                ("status_fg", p.status_fg),
                ("status_accent", p.status_accent),
                ("status_good", p.status_good),
                ("status_warn", p.status_warn),
                ("help_fg", p.help_fg),
                ("help_key_fg", p.help_key_fg),
                ("sparkline_hi", p.sparkline_hi),
                ("table_header_fg", p.table_header_fg),
                ("selection_fg", p.selection_fg),
                ("severity_ok", p.severity_ok),
                ("severity_error", p.severity_error),
                ("severity_warn", p.severity_warn),
                ("panel_title_fg", p.panel_title_fg),
                ("text_primary", p.text_primary),
                ("text_secondary", p.text_secondary),
            ];
            for (field, c) in &fg_fields {
                assert!(
                    c.r() > 0 || c.g() > 0 || c.b() > 0,
                    "theme {name}: {field} is invisible (0,0,0)"
                );
            }
        }
    }

    /// Named themes have distinct accent colors (no two share the same `tab_key_fg`).
    #[test]
    fn named_themes_are_visually_distinct() {
        let frank = TuiThemePalette::frankenstein();
        let solar = TuiThemePalette::solarized_dark();
        let drac = TuiThemePalette::dracula();
        let nord = TuiThemePalette::nord();
        let gruv = TuiThemePalette::gruvbox_dark();

        let accents = [
            ("frankenstein", frank.tab_key_fg),
            ("solarized", solar.tab_key_fg),
            ("dracula", drac.tab_key_fg),
            ("nord", nord.tab_key_fg),
            ("gruvbox", gruv.tab_key_fg),
        ];

        // Each theme should have a unique accent color.
        for i in 0..accents.len() {
            for j in (i + 1)..accents.len() {
                assert_ne!(
                    accents[i].1, accents[j].1,
                    "themes {} and {} share the same tab_key_fg",
                    accents[i].0, accents[j].0
                );
            }
        }
    }

    /// Named themes meet minimum contrast thresholds for readability.
    #[test]
    fn named_themes_meet_contrast_thresholds() {
        const MIN_TEXT: f64 = 3.0;

        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            let pairs = [
                ("tab_active", p.tab_active_fg, p.tab_active_bg),
                ("status", p.status_fg, p.status_bg),
                ("help", p.help_fg, p.help_bg),
                ("text_on_deep", p.text_primary, p.bg_deep),
                ("text_on_surface", p.text_primary, p.bg_surface),
            ];
            for (pair_name, fg, bg) in &pairs {
                let ratio = contrast_ratio(*fg, *bg);
                assert!(
                    ratio >= MIN_TEXT,
                    "theme {name}: {pair_name} contrast {ratio:.2} < {MIN_TEXT:.1}"
                );
            }
        }
    }

    /// `from_config_name` maps config strings to the correct palettes.
    #[test]
    fn from_config_name_resolves_correctly() {
        let default = TuiThemePalette::from_config_name("default");
        let frank = TuiThemePalette::frankenstein();
        assert_eq!(
            default.tab_key_fg, frank.tab_key_fg,
            "default should map to frankenstein"
        );

        let solar = TuiThemePalette::from_config_name("solarized");
        let solar_direct = TuiThemePalette::solarized_dark();
        assert_eq!(solar.tab_key_fg, solar_direct.tab_key_fg);

        let drac = TuiThemePalette::from_config_name("dracula");
        let drac_direct = TuiThemePalette::dracula();
        assert_eq!(drac.tab_key_fg, drac_direct.tab_key_fg);

        let nord = TuiThemePalette::from_config_name("nord");
        let nord_direct = TuiThemePalette::nord();
        assert_eq!(nord.tab_key_fg, nord_direct.tab_key_fg);

        let gruv = TuiThemePalette::from_config_name("gruvbox");
        let gruv_direct = TuiThemePalette::gruvbox_dark();
        assert_eq!(gruv.tab_key_fg, gruv_direct.tab_key_fg);
    }

    /// Unknown config names fall back to the default theme.
    #[test]
    fn from_config_name_unknown_falls_back() {
        let unknown = TuiThemePalette::from_config_name("matrix");
        let default = TuiThemePalette::frankenstein();
        assert_eq!(
            unknown.tab_key_fg, default.tab_key_fg,
            "unknown config name should fall back to default"
        );
    }

    /// Named theme `chart_series` arrays have 6 distinct colors.
    #[test]
    fn named_themes_chart_series_are_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            for i in 0..p.chart_series.len() {
                for j in (i + 1)..p.chart_series.len() {
                    assert_ne!(
                        p.chart_series[i], p.chart_series[j],
                        "theme {name}: chart_series[{i}] and chart_series[{j}] are identical"
                    );
                }
            }
        }
    }

    /// Named theme `agent_palette` arrays have 8 entries.
    #[test]
    fn named_themes_agent_palette_complete() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            assert_eq!(
                p.agent_palette.len(),
                8,
                "theme {name}: agent_palette should have 8 colors"
            );
            // At least 6 of 8 should be distinct (some themes may reuse a color for the last slots)
            let unique: std::collections::HashSet<u32> =
                p.agent_palette.iter().map(|c| c.0).collect();
            assert!(
                unique.len() >= 6,
                "theme {name}: agent_palette has too few distinct colors ({} of 8)",
                unique.len()
            );
        }
    }

    /// Severity colors follow the expected hierarchy for all named themes.
    #[test]
    fn named_themes_severity_hierarchy() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            // ok, warn, error should all be distinct
            assert_ne!(p.severity_ok, p.severity_warn, "theme {name}: ok == warn");
            assert_ne!(p.severity_ok, p.severity_error, "theme {name}: ok == error");
            assert_ne!(
                p.severity_warn, p.severity_error,
                "theme {name}: warn == error"
            );
        }
    }

    // ── Theme registry tests ─────────────────────────────────────

    #[test]
    fn named_theme_registry_count() {
        assert_eq!(NAMED_THEMES.len(), NAMED_THEME_COUNT);
    }

    #[test]
    fn named_theme_config_names_resolve() {
        for (i, (cfg_name, _display_name)) in NAMED_THEMES.iter().enumerate() {
            let by_name = TuiThemePalette::from_config_name(cfg_name);
            let by_index = TuiThemePalette::from_index(i);
            assert_eq!(
                by_name.tab_key_fg, by_index.tab_key_fg,
                "theme '{cfg_name}' (index {i}): from_config_name and from_index should match"
            );
        }
    }

    #[test]
    fn config_name_to_index_roundtrip() {
        for (i, (cfg_name, _)) in NAMED_THEMES.iter().enumerate() {
            assert_eq!(
                TuiThemePalette::config_name_to_index(cfg_name),
                i,
                "config_name_to_index('{cfg_name}') should return {i}"
            );
        }
        assert_eq!(TuiThemePalette::config_name_to_index("matrix"), 0);
    }

    #[test]
    fn from_index_wraps() {
        let p0 = TuiThemePalette::from_index(0);
        let p_wrap = TuiThemePalette::from_index(NAMED_THEME_COUNT);
        assert_eq!(p0.tab_key_fg, p_wrap.tab_key_fg, "index should wrap");
    }

    #[test]
    fn named_theme_display_names_unique() {
        let names: std::collections::HashSet<&str> = NAMED_THEMES.iter().map(|(_, d)| *d).collect();
        assert_eq!(
            names.len(),
            NAMED_THEME_COUNT,
            "display names should be unique"
        );
        for (_, display) in NAMED_THEMES {
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn named_theme_display_name_by_index() {
        assert_eq!(named_theme_display_name(0), "Frankenstein");
        assert_eq!(named_theme_display_name(1), "Solarized Dark");
        assert_eq!(named_theme_display_name(2), "Dracula");
        assert_eq!(named_theme_display_name(3), "Nord");
        assert_eq!(named_theme_display_name(4), "Gruvbox Dark");
    }

    #[test]
    fn init_named_theme_sets_index() {
        init_named_theme("dracula");
        assert_eq!(active_named_theme_index(), 2);
        assert_eq!(active_named_theme_config_name(), "dracula");
        assert_eq!(active_named_theme_display(), "Dracula");
        init_named_theme("default");
    }

    #[test]
    fn cycle_named_theme_wraps() {
        init_named_theme("default");
        assert_eq!(active_named_theme_index(), 0);

        let (cfg, display, _) = cycle_named_theme();
        assert_eq!(cfg, "solarized");
        assert_eq!(display, "Solarized Dark");

        let (cfg, _, _) = cycle_named_theme();
        assert_eq!(cfg, "dracula");
        let (cfg, _, _) = cycle_named_theme();
        assert_eq!(cfg, "nord");
        let (cfg, _, _) = cycle_named_theme();
        assert_eq!(cfg, "gruvbox");

        let (cfg, display, _) = cycle_named_theme();
        assert_eq!(cfg, "default");
        assert_eq!(display, "Frankenstein");

        init_named_theme("default");
    }

    #[test]
    fn set_named_theme_by_index() {
        let (cfg, display, palette) = set_named_theme(3);
        assert_eq!(cfg, "nord");
        assert_eq!(display, "Nord");
        let direct = TuiThemePalette::nord();
        assert_eq!(palette.tab_key_fg, direct.tab_key_fg);
        init_named_theme("default");
    }

    #[test]
    fn active_named_palette_matches_index() {
        init_named_theme("gruvbox");
        let palette = active_named_palette();
        let direct = TuiThemePalette::gruvbox_dark();
        assert_eq!(palette.tab_key_fg, direct.tab_key_fg);
        assert_eq!(palette.panel_bg, direct.panel_bg);
        init_named_theme("default");
    }

    // ── T14.3: Theme variant and hot-switching tests (br-3n86p) ───

    /// No named theme has any foreground token set to fully-transparent black (PackedRgba(0)).
    #[test]
    fn named_themes_no_invisible_fg_tokens() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];

        for (name, p) in &themes {
            let fg_tokens: &[(&str, PackedRgba)] = &[
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
                ("selection_fg", p.selection_fg),
                ("severity_ok", p.severity_ok),
                ("severity_error", p.severity_error),
                ("severity_warn", p.severity_warn),
                ("severity_critical", p.severity_critical),
                ("panel_border", p.panel_border),
                ("panel_border_focused", p.panel_border_focused),
                ("panel_title_fg", p.panel_title_fg),
                ("selection_indicator", p.selection_indicator),
                ("chart_axis", p.chart_axis),
                ("badge_urgent_fg", p.badge_urgent_fg),
                ("badge_info_fg", p.badge_info_fg),
                ("ttl_healthy", p.ttl_healthy),
                ("ttl_warning", p.ttl_warning),
                ("ttl_danger", p.ttl_danger),
                ("ttl_expired", p.ttl_expired),
                ("metric_uptime", p.metric_uptime),
                ("metric_requests", p.metric_requests),
                ("metric_latency", p.metric_latency),
                ("metric_messages", p.metric_messages),
                ("metric_agents", p.metric_agents),
                ("contact_approved", p.contact_approved),
                ("contact_pending", p.contact_pending),
                ("contact_blocked", p.contact_blocked),
                ("activity_active", p.activity_active),
                ("activity_idle", p.activity_idle),
                ("activity_stale", p.activity_stale),
                ("text_primary", p.text_primary),
                ("text_secondary", p.text_secondary),
            ];
            for (field, c) in fg_tokens {
                assert_ne!(c.0, 0, "theme {name}: {field} is PackedRgba(0) (invisible)");
            }
        }
    }

    /// Default theme matches frankenstein exactly — regression guard.
    #[test]
    fn default_theme_matches_frankenstein_production() {
        let default = TuiThemePalette::from_config_name("default");
        let frank = TuiThemePalette::frankenstein();

        assert_eq!(default.tab_active_fg, frank.tab_active_fg);
        assert_eq!(default.tab_active_bg, frank.tab_active_bg);
        assert_eq!(default.tab_inactive_fg, frank.tab_inactive_fg);
        assert_eq!(default.tab_key_fg, frank.tab_key_fg);
        assert_eq!(default.status_fg, frank.status_fg);
        assert_eq!(default.status_bg, frank.status_bg);
        assert_eq!(default.status_accent, frank.status_accent);
        assert_eq!(default.status_good, frank.status_good);
        assert_eq!(default.status_warn, frank.status_warn);
        assert_eq!(default.help_fg, frank.help_fg);
        assert_eq!(default.help_bg, frank.help_bg);
        assert_eq!(default.help_key_fg, frank.help_key_fg);
        assert_eq!(default.sparkline_lo, frank.sparkline_lo);
        assert_eq!(default.sparkline_hi, frank.sparkline_hi);
        assert_eq!(default.table_header_fg, frank.table_header_fg);
        assert_eq!(default.selection_fg, frank.selection_fg);
        assert_eq!(default.selection_bg, frank.selection_bg);
        assert_eq!(default.severity_ok, frank.severity_ok);
        assert_eq!(default.severity_error, frank.severity_error);
        assert_eq!(default.severity_warn, frank.severity_warn);
        assert_eq!(default.severity_critical, frank.severity_critical);
        assert_eq!(default.panel_bg, frank.panel_bg);
        assert_eq!(default.panel_border, frank.panel_border);
        assert_eq!(default.text_primary, frank.text_primary);
        assert_eq!(default.bg_deep, frank.bg_deep);
        assert_eq!(default.bg_surface, frank.bg_surface);
        assert_eq!(default.chart_series, frank.chart_series);
        assert_eq!(default.agent_palette, frank.agent_palette);
    }

    /// Theme cycling via `cycle_named_theme` completes in under 1ms.
    #[test]
    fn cycle_named_theme_sub_millisecond() {
        init_named_theme("default");
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let _ = cycle_named_theme();
        }
        let elapsed = start.elapsed();
        // 100 cycles should complete well under 100ms (< 1ms each).
        assert!(
            elapsed.as_millis() < 100,
            "100 theme cycles took {elapsed:?}, expected < 100ms"
        );
        init_named_theme("default");
    }

    /// Selection fg/bg pairs meet contrast threshold across all named themes.
    #[test]
    fn named_themes_selection_contrast() {
        const MIN_CONTRAST: f64 = 3.0;
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let ratio = contrast_ratio(p.selection_fg, p.selection_bg);
            assert!(
                ratio >= MIN_CONTRAST,
                "theme {name}: selection contrast {ratio:.2} < {MIN_CONTRAST:.1}"
            );
        }
    }

    /// Severity colors on `panel_bg` meet contrast threshold.
    #[test]
    fn named_themes_severity_on_panel_contrast() {
        const MIN_CONTRAST: f64 = 2.5;
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let pairs = [
                ("severity_ok", p.severity_ok),
                ("severity_warn", p.severity_warn),
                ("severity_error", p.severity_error),
                ("severity_critical", p.severity_critical),
            ];
            for (pair_name, fg) in &pairs {
                let ratio = contrast_ratio(*fg, p.panel_bg);
                assert!(
                    ratio >= MIN_CONTRAST,
                    "theme {name}: {pair_name} on panel_bg contrast {ratio:.2} < {MIN_CONTRAST:.1}"
                );
            }
        }
    }

    /// Badge fg/bg pairs meet contrast threshold (lower bar since badges are small + colorful).
    #[test]
    fn named_themes_badge_contrast() {
        const MIN_CONTRAST: f64 = 1.5;
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let urgent = contrast_ratio(p.badge_urgent_fg, p.badge_urgent_bg);
            let info = contrast_ratio(p.badge_info_fg, p.badge_info_bg);
            assert!(
                urgent >= MIN_CONTRAST,
                "theme {name}: badge_urgent contrast {urgent:.2} < {MIN_CONTRAST:.1}"
            );
            assert!(
                info >= MIN_CONTRAST,
                "theme {name}: badge_info contrast {info:.2} < {MIN_CONTRAST:.1}"
            );
        }
    }

    /// `init_named_theme` with each valid config name sets the correct index.
    #[test]
    fn init_named_theme_all_config_names() {
        for (expected_idx, (cfg_name, _)) in NAMED_THEMES.iter().enumerate() {
            init_named_theme(cfg_name);
            assert_eq!(
                active_named_theme_index(),
                expected_idx,
                "init_named_theme('{cfg_name}') should set index to {expected_idx}"
            );
        }
        init_named_theme("default");
    }

    /// Invalid config name falls back to index 0 (default) via `init_named_theme`.
    #[test]
    fn init_named_theme_invalid_falls_back() {
        init_named_theme("nonexistent_theme");
        assert_eq!(active_named_theme_index(), 0);
        assert_eq!(active_named_theme_config_name(), "default");
        init_named_theme("default");
    }

    /// `TuiThemePalette::current()` updates after `cycle_named_theme`.
    #[test]
    fn current_updates_after_cycle() {
        init_named_theme("default");
        let before = TuiThemePalette::current();

        let (_, _, cycled_palette) = cycle_named_theme();
        let after = TuiThemePalette::current();

        // After cycling from default to solarized, palette should change.
        assert_ne!(
            before.tab_key_fg, after.tab_key_fg,
            "current() should reflect new palette after cycle"
        );
        assert_eq!(
            cycled_palette.tab_key_fg, after.tab_key_fg,
            "current() should match the palette returned by cycle"
        );
        init_named_theme("default");
    }

    /// `set_named_theme` wraps on out-of-bounds index.
    #[test]
    fn set_named_theme_wraps_on_overflow() {
        let (cfg, _, _) = set_named_theme(NAMED_THEME_COUNT + 2);
        let expected_idx = (NAMED_THEME_COUNT + 2) % NAMED_THEME_COUNT;
        assert_eq!(
            cfg,
            named_theme_config_name(expected_idx),
            "set_named_theme should wrap index"
        );
        init_named_theme("default");
    }

    /// TTL band colors are all distinct within each theme.
    #[test]
    fn named_themes_ttl_bands_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let bands = [p.ttl_healthy, p.ttl_warning, p.ttl_danger, p.ttl_expired];
            for i in 0..bands.len() {
                for j in (i + 1)..bands.len() {
                    assert_ne!(
                        bands[i], bands[j],
                        "theme {name}: ttl band [{i}] and [{j}] are identical"
                    );
                }
            }
        }
    }

    /// Contact status colors are all distinct within each theme.
    #[test]
    fn named_themes_contact_colors_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            assert_ne!(
                p.contact_approved, p.contact_pending,
                "theme {name}: approved == pending"
            );
            assert_ne!(
                p.contact_approved, p.contact_blocked,
                "theme {name}: approved == blocked"
            );
            assert_ne!(
                p.contact_pending, p.contact_blocked,
                "theme {name}: pending == blocked"
            );
        }
    }

    /// Activity recency colors form a distinguishable gradient.
    #[test]
    fn named_themes_activity_recency_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            assert_ne!(
                p.activity_active, p.activity_idle,
                "theme {name}: active == idle"
            );
            assert_ne!(
                p.activity_active, p.activity_stale,
                "theme {name}: active == stale"
            );
            assert_ne!(
                p.activity_idle, p.activity_stale,
                "theme {name}: idle == stale"
            );
        }
    }

    /// Metric tile colors are all distinct within each theme.
    #[test]
    fn named_themes_metric_tile_colors_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let metrics = [
                ("uptime", p.metric_uptime),
                ("requests", p.metric_requests),
                ("latency", p.metric_latency),
                ("messages", p.metric_messages),
                ("agents", p.metric_agents),
            ];
            for i in 0..metrics.len() {
                for j in (i + 1)..metrics.len() {
                    assert_ne!(
                        metrics[i].1, metrics[j].1,
                        "theme {name}: metric {} and {} are identical",
                        metrics[i].0, metrics[j].0
                    );
                }
            }
        }
    }

    /// `text_muted` and `text_disabled` differ from `text_primary` in all themes.
    #[test]
    fn named_themes_text_hierarchy_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            assert_ne!(
                p.text_primary, p.text_muted,
                "theme {name}: primary == muted"
            );
            assert_ne!(
                p.text_primary, p.text_disabled,
                "theme {name}: primary == disabled"
            );
            assert_ne!(
                p.text_primary, p.text_secondary,
                "theme {name}: primary == secondary"
            );
        }
    }

    /// Toast notification colors (error, warning, info, success) are distinct in each theme.
    #[test]
    fn named_themes_toast_colors_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let toasts = [
                ("error", p.toast_error),
                ("warning", p.toast_warning),
                ("info", p.toast_info),
                ("success", p.toast_success),
            ];
            for i in 0..toasts.len() {
                for j in (i + 1)..toasts.len() {
                    assert_ne!(
                        toasts[i].1, toasts[j].1,
                        "theme {name}: toast {} and {} are identical",
                        toasts[i].0, toasts[j].0
                    );
                }
            }
        }
    }

    /// JSON token colors (key, string, number, literal, punctuation) are distinct in each theme.
    #[test]
    fn named_themes_json_token_colors_distinct() {
        let themes: [(&str, TuiThemePalette); 5] = [
            ("frankenstein", TuiThemePalette::frankenstein()),
            ("solarized_dark", TuiThemePalette::solarized_dark()),
            ("dracula", TuiThemePalette::dracula()),
            ("nord", TuiThemePalette::nord()),
            ("gruvbox_dark", TuiThemePalette::gruvbox_dark()),
        ];
        for (name, p) in &themes {
            let tokens = [
                ("key", p.json_key),
                ("string", p.json_string),
                ("number", p.json_number),
                ("literal", p.json_literal),
                ("punctuation", p.json_punctuation),
            ];
            for i in 0..tokens.len() {
                for j in (i + 1)..tokens.len() {
                    assert_ne!(
                        tokens[i].1, tokens[j].1,
                        "theme {name}: json {} and {} are identical",
                        tokens[i].0, tokens[j].0
                    );
                }
            }
        }
    }
}
