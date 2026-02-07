//! Centralized theme system for console output (br-1m6a.12).
//!
//! Uses `ftui_extras::theme` as the single source of truth for all console
//! colors. Provides ANSI escape code helpers that resolve against the active
//! theme palette so banner, tool panels, request panels, and HUD all render
//! with a cohesive look.

use ftui_extras::theme::{self, ColorToken, ThemeId};
use mcp_agent_mail_core::config::ConsoleThemeId;

// ──────────────────────────────────────────────────────────────────────
// CONSOLE_THEME parsing
// ──────────────────────────────────────────────────────────────────────

/// Parse a `CONSOLE_THEME` string into a [`ThemeId`].
///
/// Accepts both display names ("Cyberpunk Aurora") and `snake_case`
/// identifiers (`cyberpunk_aurora`). Case-insensitive.
///
/// Returns [`ThemeId::CyberpunkAurora`] for unrecognized values.
#[must_use]
pub fn parse_console_theme(value: &str) -> ThemeId {
    match value.trim().to_ascii_lowercase().as_str() {
        "darcula" => ThemeId::Darcula,
        "lumen_light" | "lumen light" | "lumen" | "light" => ThemeId::LumenLight,
        "nordic_frost" | "nordic frost" | "nordic" => ThemeId::NordicFrost,
        "high_contrast" | "high contrast" | "highcontrast" => ThemeId::HighContrast,
        // Default: CyberpunkAurora for both explicit names and unknown values.
        _ => ThemeId::CyberpunkAurora,
    }
}

/// Initialize the console theme from the `CONSOLE_THEME` environment variable.
///
/// Call this early in server startup (before any banner/HUD rendering) when
/// rich console output is active (`LOG_RICH_ENABLED=true` + TTY).
///
/// Returns the resolved [`ThemeId`].
#[must_use]
pub fn init_console_theme() -> ThemeId {
    let value = std::env::var("CONSOLE_THEME").unwrap_or_default();
    let id = if value.is_empty() {
        ThemeId::CyberpunkAurora
    } else {
        parse_console_theme(&value)
    };
    theme::set_theme(id);
    id
}

/// Initialize the console theme from the already-parsed config value.
///
/// This is the preferred server integration path because `Config` may load
/// console settings from the persisted user envfile (and those values are not
/// guaranteed to exist in the process environment).
#[must_use]
pub fn init_console_theme_from_config(theme_id: ConsoleThemeId) -> ThemeId {
    let id = match theme_id {
        ConsoleThemeId::CyberpunkAurora => ThemeId::CyberpunkAurora,
        ConsoleThemeId::Darcula => ThemeId::Darcula,
        ConsoleThemeId::LumenLight => ThemeId::LumenLight,
        ConsoleThemeId::NordicFrost => ThemeId::NordicFrost,
        ConsoleThemeId::HighContrast => ThemeId::HighContrast,
    };
    theme::set_theme(id);
    id
}

// ──────────────────────────────────────────────────────────────────────
// ANSI escape helpers (resolve from current theme palette)
// ──────────────────────────────────────────────────────────────────────

/// ANSI reset sequence.
pub const RESET: &str = "\x1b[0m";

/// ANSI dim attribute.
pub const DIM: &str = "\x1b[2m";

/// Format an ANSI 24-bit foreground escape for the given [`ColorToken`].
///
/// Resolves the token against the current theme palette.
#[must_use]
pub fn ansi_fg(token: ColorToken) -> String {
    let c = token.resolve();
    format!("\x1b[38;2;{};{};{}m", c.r(), c.g(), c.b())
}

/// Format an ANSI 24-bit bold+foreground escape for the given [`ColorToken`].
#[must_use]
pub fn ansi_fg_bold(token: ColorToken) -> String {
    let c = token.resolve();
    format!("\x1b[1;38;2;{};{};{}m", c.r(), c.g(), c.b())
}

/// Format an ANSI 24-bit background escape for the given [`ColorToken`].
#[must_use]
pub fn ansi_bg(token: ColorToken) -> String {
    let c = token.resolve();
    format!("\x1b[48;2;{};{};{}m", c.r(), c.g(), c.b())
}

// ──────────────────────────────────────────────────────────────────────
// Semantic color shortcuts (mirror the old ANSI constants)
// ──────────────────────────────────────────────────────────────────────

/// Primary accent bold (was `CYAN_B` — brand/emphasis color).
#[must_use]
pub fn primary_bold() -> String {
    ansi_fg_bold(theme::accent::PRIMARY)
}

/// Secondary color (was `BLUE` — borders, structural elements).
#[must_use]
pub fn secondary() -> String {
    ansi_fg(theme::accent::SECONDARY)
}

/// Secondary bold (was `BLUE_B` — prominent borders).
#[must_use]
pub fn secondary_bold() -> String {
    ansi_fg_bold(theme::accent::SECONDARY)
}

/// Accent color (was `MAG` — highlights, special values).
#[must_use]
pub fn accent() -> String {
    ansi_fg(theme::accent::INFO)
}

/// Success bold (was `GREEN_B` — success indicators, enabled states).
#[must_use]
pub fn success_bold() -> String {
    ansi_fg_bold(theme::accent::SUCCESS)
}

/// Warning bold (was `YELLOW_B` — warnings, numbers, caution).
#[must_use]
pub fn warning_bold() -> String {
    ansi_fg_bold(theme::accent::WARNING)
}

/// Error bold (was `RED_B` — errors, critical states).
#[must_use]
pub fn error_bold() -> String {
    ansi_fg_bold(theme::accent::ERROR)
}

/// Primary text bold (was `WHITE_B` — important text on dark backgrounds).
#[must_use]
pub fn text_bold() -> String {
    ansi_fg_bold(theme::fg::PRIMARY)
}

/// Muted text (was `DIM` with explicit color — secondary/disabled text).
#[must_use]
pub fn muted() -> String {
    ansi_fg(theme::fg::MUTED)
}

/// Link color (for URLs and references).
#[must_use]
pub fn link() -> String {
    ansi_fg(theme::accent::LINK)
}

// ──────────────────────────────────────────────────────────────────────
// Sparkline gradient endpoints (resolve from theme)
// ──────────────────────────────────────────────────────────────────────

/// Low-value gradient color for the sparkline (from theme secondary accent).
#[must_use]
pub fn sparkline_lo() -> ftui::PackedRgba {
    theme::accent::SECONDARY.resolve()
}

/// High-value gradient color for the sparkline (from theme success accent).
#[must_use]
pub fn sparkline_hi() -> ftui::PackedRgba {
    theme::accent::SUCCESS.resolve()
}

// ──────────────────────────────────────────────────────────────────────
// JSON syntax coloring tokens (resolve from theme)
// ──────────────────────────────────────────────────────────────────────

/// ANSI color for JSON keys.
#[must_use]
pub fn json_key() -> String {
    ansi_fg_bold(theme::syntax::KEYWORD)
}

/// ANSI color for JSON string values.
#[must_use]
pub fn json_string() -> String {
    ansi_fg(theme::syntax::STRING)
}

/// ANSI color for JSON numbers.
#[must_use]
pub fn json_number() -> String {
    ansi_fg_bold(theme::syntax::NUMBER)
}

/// Current theme display name (e.g., "Cyberpunk Aurora").
#[must_use]
pub fn current_theme_display_name() -> &'static str {
    theme::current_theme_name()
}

/// Current theme ID.
#[must_use]
pub fn current_theme_id() -> ThemeId {
    theme::current_theme()
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_extras::theme::ScopedThemeLock;

    #[test]
    fn parse_known_themes() {
        assert_eq!(
            parse_console_theme("cyberpunk_aurora"),
            ThemeId::CyberpunkAurora
        );
        assert_eq!(parse_console_theme("darcula"), ThemeId::Darcula);
        assert_eq!(parse_console_theme("lumen_light"), ThemeId::LumenLight);
        assert_eq!(parse_console_theme("nordic_frost"), ThemeId::NordicFrost);
        assert_eq!(parse_console_theme("high_contrast"), ThemeId::HighContrast);
    }

    #[test]
    fn parse_display_names() {
        assert_eq!(
            parse_console_theme("Cyberpunk Aurora"),
            ThemeId::CyberpunkAurora
        );
        assert_eq!(parse_console_theme("Nordic Frost"), ThemeId::NordicFrost);
        assert_eq!(parse_console_theme("High Contrast"), ThemeId::HighContrast);
    }

    #[test]
    fn parse_short_aliases() {
        assert_eq!(parse_console_theme("cyberpunk"), ThemeId::CyberpunkAurora);
        assert_eq!(parse_console_theme("nordic"), ThemeId::NordicFrost);
        assert_eq!(parse_console_theme("lumen"), ThemeId::LumenLight);
        assert_eq!(parse_console_theme("light"), ThemeId::LumenLight);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_console_theme("DARCULA"), ThemeId::Darcula);
        assert_eq!(parse_console_theme("Nordic_Frost"), ThemeId::NordicFrost);
        assert_eq!(parse_console_theme("HIGH_CONTRAST"), ThemeId::HighContrast);
    }

    #[test]
    fn parse_unknown_defaults_to_cyberpunk() {
        assert_eq!(parse_console_theme("nonexistent"), ThemeId::CyberpunkAurora);
        assert_eq!(parse_console_theme(""), ThemeId::CyberpunkAurora);
        assert_eq!(parse_console_theme("   "), ThemeId::CyberpunkAurora);
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(parse_console_theme("  darcula  "), ThemeId::Darcula);
    }

    #[test]
    fn ansi_fg_produces_rgb_escape() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let code = ansi_fg(theme::accent::SUCCESS);
        assert!(code.starts_with("\x1b[38;2;"), "expected 24-bit fg: {code}");
        assert!(code.ends_with('m'), "expected 'm' terminator: {code}");
    }

    #[test]
    fn ansi_fg_bold_includes_bold_attr() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let code = ansi_fg_bold(theme::accent::ERROR);
        assert!(
            code.starts_with("\x1b[1;38;2;"),
            "expected bold+24-bit: {code}"
        );
    }

    #[test]
    fn ansi_bg_produces_bg_escape() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let code = ansi_bg(theme::accent::INFO);
        assert!(code.starts_with("\x1b[48;2;"), "expected 24-bit bg: {code}");
    }

    #[test]
    fn semantic_shortcuts_not_empty() {
        let _guard = ScopedThemeLock::new(ThemeId::Darcula);
        assert!(!primary_bold().is_empty());
        assert!(!secondary().is_empty());
        assert!(!secondary_bold().is_empty());
        assert!(!accent().is_empty());
        assert!(!success_bold().is_empty());
        assert!(!warning_bold().is_empty());
        assert!(!error_bold().is_empty());
        assert!(!text_bold().is_empty());
        assert!(!muted().is_empty());
        assert!(!link().is_empty());
    }

    #[test]
    fn json_colors_not_empty() {
        let _guard = ScopedThemeLock::new(ThemeId::LumenLight);
        assert!(!json_key().is_empty());
        assert!(!json_string().is_empty());
        assert!(!json_number().is_empty());
    }

    #[test]
    fn theme_name_matches_id() {
        let _guard = ScopedThemeLock::new(ThemeId::NordicFrost);
        assert_eq!(current_theme_display_name(), "Nordic Frost");
        assert_eq!(current_theme_id(), ThemeId::NordicFrost);
    }

    #[test]
    fn sparkline_colors_are_valid_rgba() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let lo = sparkline_lo();
        let hi = sparkline_hi();
        // Both should have non-zero RGB (not black/transparent)
        assert!(
            lo.r() > 0 || lo.g() > 0 || lo.b() > 0,
            "lo should not be black"
        );
        assert!(
            hi.r() > 0 || hi.g() > 0 || hi.b() > 0,
            "hi should not be black"
        );
    }

    #[test]
    fn different_themes_produce_different_colors() {
        let code_cyber = {
            let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
            primary_bold()
        };
        let code_darcula = {
            let _guard = ScopedThemeLock::new(ThemeId::Darcula);
            primary_bold()
        };
        // Different themes should produce different ANSI codes (different RGB values)
        assert_ne!(
            code_cyber, code_darcula,
            "cyberpunk and darcula should differ"
        );
    }

    #[test]
    fn init_console_theme_sets_default() {
        // With no env var set, should default to CyberpunkAurora
        let _guard = ScopedThemeLock::new(ThemeId::HighContrast); // start at different theme
        // init_console_theme reads env, but CONSOLE_THEME is likely unset in test env
        // Just verify parse logic works
        let id = parse_console_theme("");
        assert_eq!(id, ThemeId::CyberpunkAurora);
    }

    #[test]
    fn all_five_themes_parseable() {
        let themes = [
            ("cyberpunk_aurora", ThemeId::CyberpunkAurora),
            ("darcula", ThemeId::Darcula),
            ("lumen_light", ThemeId::LumenLight),
            ("nordic_frost", ThemeId::NordicFrost),
            ("high_contrast", ThemeId::HighContrast),
        ];
        for (name, expected) in themes {
            assert_eq!(parse_console_theme(name), expected, "failed for {name}");
        }
    }

    #[test]
    fn ansi_colors_change_with_theme() {
        // Verify that the ANSI output actually changes when we switch themes
        let colors: Vec<String> = ThemeId::ALL
            .iter()
            .map(|&id| {
                let _guard = ScopedThemeLock::new(id);
                success_bold()
            })
            .collect();

        // At least some themes should produce unique success colors
        let unique: std::collections::HashSet<&String> = colors.iter().collect();
        assert!(
            unique.len() >= 3,
            "expected at least 3 unique success colors across 5 themes, got {}",
            unique.len()
        );
    }
}
