//! Theme integration: map ftui theme palettes to TUI-specific styles.
//!
//! Resolves the active `ftui_extras::theme` palette into a
//! [`TuiThemePalette`] struct that every TUI component can query for
//! consistent, theme-aware colors.

use ftui::{PackedRgba, Style};
use ftui_extras::theme::{self, ThemeId};

use crate::tui_events::{EventSeverity, MailEventKind};

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
}

impl TuiThemePalette {
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
        }
    }

    /// Resolve a palette from the currently active ftui theme.
    #[must_use]
    pub fn current() -> Self {
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
    theme::cycle_theme();
    theme::current_theme_name()
}

/// Get the current theme display name.
#[must_use]
pub fn current_theme_name() -> &'static str {
    theme::current_theme_name()
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_extras::theme::ScopedThemeLock;

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
}
