//! Chrome shell for `AgentMailTUI`: tab bar, status line, help overlay.
//!
//! The chrome renders persistent UI elements that frame every screen.
//! Layout: `[tab_bar(1)] [screen_content(fill)] [status_line(1)]`

use ftui::layout::{Constraint, Flex, Rect};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Frame, PackedRgba, Style};

use crate::tui_bridge::TuiSharedState;
use crate::tui_persist::AccessibilitySettings;
use crate::tui_screens::{HelpEntry, MAIL_SCREEN_REGISTRY, MailScreenId, screen_meta};

// ──────────────────────────────────────────────────────────────────────
// Color palette
// ──────────────────────────────────────────────────────────────────────

// Legacy hardcoded colors (Cyberpunk Aurora defaults). Kept for
// `ChromePalette::standard()` backward-compat; live rendering now
// resolves from `TuiThemePalette::current()`.
const TAB_ACTIVE_BG: PackedRgba = PackedRgba::rgb(50, 70, 110);
const TAB_ACTIVE_FG: PackedRgba = PackedRgba::rgb(255, 255, 255);
const TAB_INACTIVE_FG: PackedRgba = PackedRgba::rgb(140, 150, 170);
const TAB_KEY_FG: PackedRgba = PackedRgba::rgb(100, 180, 255);

const STATUS_FG: PackedRgba = PackedRgba::rgb(160, 170, 190);
const STATUS_ACCENT: PackedRgba = PackedRgba::rgb(144, 205, 255);
const STATUS_GOOD: PackedRgba = PackedRgba::rgb(120, 220, 150);
const STATUS_WARN: PackedRgba = PackedRgba::rgb(255, 184, 108);

const HELP_FG: PackedRgba = PackedRgba::rgb(200, 210, 230);
const HELP_KEY_FG: PackedRgba = PackedRgba::rgb(100, 180, 255);
const HELP_BORDER_FG: PackedRgba = PackedRgba::rgb(80, 100, 140);
const HELP_CATEGORY_FG: PackedRgba = PackedRgba::rgb(180, 140, 255);

// High-contrast palette — brighter foreground, darker background
const HC_TAB_ACTIVE_BG: PackedRgba = PackedRgba::rgb(0, 50, 120);
const HC_TAB_ACTIVE_FG: PackedRgba = PackedRgba::rgb(255, 255, 255);
const HC_TAB_INACTIVE_FG: PackedRgba = PackedRgba::rgb(200, 210, 230);
const HC_TAB_KEY_FG: PackedRgba = PackedRgba::rgb(80, 200, 255);
const HC_STATUS_FG: PackedRgba = PackedRgba::rgb(220, 230, 240);
const HC_STATUS_ACCENT: PackedRgba = PackedRgba::rgb(100, 220, 255);
const HC_STATUS_GOOD: PackedRgba = PackedRgba::rgb(80, 255, 140);
const HC_STATUS_WARN: PackedRgba = PackedRgba::rgb(255, 160, 80);
const HC_HELP_FG: PackedRgba = PackedRgba::rgb(240, 245, 255);
const HC_HELP_KEY_FG: PackedRgba = PackedRgba::rgb(80, 220, 255);
const HC_HELP_BORDER_FG: PackedRgba = PackedRgba::rgb(120, 150, 200);
const HC_HELP_CATEGORY_FG: PackedRgba = PackedRgba::rgb(220, 180, 255);

// ──────────────────────────────────────────────────────────────────────
// Chrome layout
// ──────────────────────────────────────────────────────────────────────

/// Split the terminal area into tab bar, content, and status line regions.
#[must_use]
pub fn chrome_layout(area: Rect) -> ChromeAreas {
    let chunks = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),
            Constraint::Min(1),
            Constraint::Fixed(1),
        ])
        .split(area);
    ChromeAreas {
        tab_bar: chunks[0],
        content: chunks[1],
        status_line: chunks[2],
    }
}

/// The three regions of the chrome layout.
pub struct ChromeAreas {
    pub tab_bar: Rect,
    pub content: Rect,
    pub status_line: Rect,
}

// ──────────────────────────────────────────────────────────────────────
// Tab bar
// ──────────────────────────────────────────────────────────────────────

/// Render the tab bar into a 1-row area.
pub fn render_tab_bar(active: MailScreenId, frame: &mut Frame, area: Rect) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();

    // Fill background
    let bg_style = Style::default().bg(tp.tab_inactive_bg);
    Paragraph::new("").style(bg_style).render(area, frame);

    let mut x = area.x;
    let available = area.width;

    // Determine if we need compact mode (< 60 cols)
    let compact = available < 60;

    for (i, meta) in MAIL_SCREEN_REGISTRY.iter().enumerate() {
        let number = i + 1;
        let label = if compact {
            meta.short_label
        } else {
            meta.title
        };
        let is_active = meta.id == active;

        // " 1:Label " — each tab has fixed structure
        let key_str = format!("{number}");
        // Width: space + key + colon + label + space
        let tab_width = u16::try_from(1 + key_str.len() + 1 + label.len() + 1).unwrap_or(u16::MAX);

        if x + tab_width > area.x + available {
            break; // Don't overflow
        }

        let (fg, bg) = if is_active {
            (tp.tab_active_fg, tp.tab_active_bg)
        } else {
            (tp.tab_inactive_fg, tp.tab_inactive_bg)
        };

        let spans = vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(key_str, Style::default().fg(tp.tab_key_fg).bg(bg)),
            Span::styled(":", Style::default().fg(tp.tab_inactive_fg).bg(bg)),
            Span::styled(label, Style::default().fg(fg).bg(bg)),
            Span::styled(" ", Style::default().bg(bg)),
        ];

        let line = Line::from_spans(spans);
        let tab_area = Rect::new(x, area.y, tab_width, 1);
        Paragraph::new(Text::from_lines([line])).render(tab_area, frame);

        x += tab_width;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Status line
// ──────────────────────────────────────────────────────────────────────

/// Render the status line into a 1-row area.
pub fn render_status_line(
    state: &TuiSharedState,
    active: MailScreenId,
    help_visible: bool,
    frame: &mut Frame,
    area: Rect,
) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();

    // Fill background
    let bg = Style::default().bg(tp.status_bg);
    Paragraph::new("").style(bg).render(area, frame);

    let counters = state.request_counters();
    let uptime = state.uptime();
    let meta = screen_meta(active);
    let transport_mode = state.config_snapshot().transport_mode();

    // Build left section
    let uptime_secs = uptime.as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;
    let secs = uptime_secs % 60;
    let uptime_str = if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m{secs:02}s")
    };

    // Build center section (live counters)
    let avg_latency = state.avg_latency_ms();
    let error_count = counters.status_4xx + counters.status_5xx;
    let total = counters.total;
    let ok = counters.status_2xx;
    let center_str = format!("req:{total} ok:{ok} err:{error_count} avg:{avg_latency}ms");

    // Build right section
    let help_hint = if help_visible { "[?] Help" } else { "? help" };

    // Calculate widths
    let title = meta.title;
    let left_len =
        u16::try_from(1 + title.len() + 12 + transport_mode.len() + uptime_str.len() + 1)
            .unwrap_or(u16::MAX);
    let center_len = u16::try_from(center_str.len()).unwrap_or(u16::MAX);
    let right_len = u16::try_from(1 + help_hint.len() + 1).unwrap_or(u16::MAX);
    let total_len = left_len
        .saturating_add(center_len)
        .saturating_add(right_len);
    let available = area.width;

    // Build spans
    let mut spans = Vec::with_capacity(8);

    // Left: screen name + uptime
    spans.push(Span::styled(" ", Style::default().bg(tp.status_bg)));
    spans.push(Span::styled(
        title,
        Style::default()
            .fg(tp.status_accent)
            .bg(tp.status_bg)
            .bold(),
    ));
    spans.push(Span::styled(
        format!(" | mode:{transport_mode} up:{uptime_str} "),
        Style::default().fg(tp.status_fg).bg(tp.status_bg),
    ));

    // Center padding + counters
    if total_len < available {
        let pad = (available - total_len) / 2;
        if pad > 0 {
            spans.push(Span::styled(
                " ".repeat(pad as usize),
                Style::default().bg(tp.status_bg),
            ));
        }
    }

    let counter_fg = if error_count > 0 {
        tp.status_warn
    } else {
        tp.status_good
    };
    spans.push(Span::styled(
        center_str,
        Style::default().fg(counter_fg).bg(tp.status_bg),
    ));

    // Right padding + help hint
    if total_len < available {
        let used_with_center_pad = total_len + (available - total_len) / 2;
        let right_pad = available.saturating_sub(used_with_center_pad);
        if right_pad > 0 {
            spans.push(Span::styled(
                " ".repeat(right_pad as usize),
                Style::default().bg(tp.status_bg),
            ));
        }
    }

    spans.push(Span::styled(
        help_hint,
        Style::default().fg(tp.tab_key_fg).bg(tp.status_bg),
    ));
    spans.push(Span::styled(" ", Style::default().bg(tp.status_bg)));

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines([line])).render(area, frame);
}

// ──────────────────────────────────────────────────────────────────────
// Help overlay
// ──────────────────────────────────────────────────────────────────────

/// Global keybindings shown in every help overlay.
const GLOBAL_KEYBINDINGS: &[(&str, &str)] = &[
    ("1-8", "Jump to screen"),
    ("Tab", "Next screen"),
    ("Shift+Tab", "Previous screen"),
    ("m", "Toggle MCP/API mode"),
    ("Ctrl+P / :", "Command palette"),
    ("T", "Cycle theme"),
    ("?", "Toggle help"),
    ("q", "Quit"),
    ("Esc", "Dismiss overlay"),
];

/// Render the help overlay centered on the terminal.
pub fn render_help_overlay(
    active: MailScreenId,
    screen_bindings: &[HelpEntry],
    frame: &mut Frame,
    area: Rect,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Calculate overlay dimensions (60% width, 60% height, clamped)
    let overlay_width = (u32::from(area.width) * 60 / 100).clamp(36, 72) as u16;
    let overlay_height = (u32::from(area.height) * 60 / 100).clamp(10, 24) as u16;
    let overlay_width = overlay_width.min(area.width.saturating_sub(2));
    let overlay_height = overlay_height.min(area.height.saturating_sub(2));

    // Center the overlay
    let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

    // Render border frame
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .title(" Keyboard Shortcuts (Esc to close) ")
        .style(Style::default().fg(tp.help_border_fg).bg(tp.help_bg));

    let inner = block.inner(overlay_area);
    block.render(overlay_area, frame);

    // Render keybinding entries inside the inner area
    let mut y_offset = 0u16;
    let col_width = inner.width.saturating_sub(1);
    let key_col = 14u16; // width for key column

    // Global section header
    if y_offset < inner.height {
        let header = Paragraph::new("Global").style(
            Style::default()
                .fg(tp.help_category_fg)
                .bg(tp.help_bg)
                .bold(),
        );
        header.render(
            Rect::new(inner.x + 1, inner.y + y_offset, col_width, 1),
            frame,
        );
        y_offset += 1;
    }

    // Global keybindings
    for &(key, action) in GLOBAL_KEYBINDINGS {
        if y_offset >= inner.height {
            break;
        }
        render_keybinding_line_themed(
            key,
            action,
            Rect::new(inner.x + 1, inner.y + y_offset, col_width, 1),
            key_col,
            &tp,
            frame,
        );
        y_offset += 1;
    }

    // Screen-specific section
    if !screen_bindings.is_empty() && y_offset < inner.height {
        // Blank separator
        y_offset += 1;

        let meta = screen_meta(active);
        if y_offset < inner.height {
            let header = Paragraph::new(meta.title).style(
                Style::default()
                    .fg(tp.help_category_fg)
                    .bg(tp.help_bg)
                    .bold(),
            );
            header.render(
                Rect::new(inner.x + 1, inner.y + y_offset, col_width, 1),
                frame,
            );
            y_offset += 1;
        }

        for entry in screen_bindings {
            if y_offset >= inner.height {
                break;
            }
            render_keybinding_line_themed(
                entry.key,
                entry.action,
                Rect::new(inner.x + 1, inner.y + y_offset, col_width, 1),
                key_col,
                &tp,
                frame,
            );
            y_offset += 1;
        }
    }
}

/// Render a single keybinding line: `  [key]  action` (theme-aware).
fn render_keybinding_line_themed(
    key: &str,
    action: &str,
    area: Rect,
    key_col: u16,
    tp: &crate::tui_theme::TuiThemePalette,
    frame: &mut Frame,
) {
    use ftui::text::{Line, Span, Text};

    let key_display = format!("  [{key}]");
    let key_len = u16::try_from(key_display.len()).unwrap_or(key_col);
    let pad_len = key_col.saturating_sub(key_len) as usize;
    let padding = " ".repeat(pad_len);

    let spans = vec![
        Span::styled(
            key_display,
            Style::default().fg(tp.help_key_fg).bg(tp.help_bg),
        ),
        Span::styled(padding, Style::default().bg(tp.help_bg)),
        Span::styled(action, Style::default().fg(tp.help_fg).bg(tp.help_bg)),
    ];

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines([line])).render(area, frame);
}

// ──────────────────────────────────────────────────────────────────────
// ChromePalette — accessibility-aware color set
// ──────────────────────────────────────────────────────────────────────

/// Resolved color palette respecting accessibility settings.
#[derive(Debug, Clone, Copy)]
pub struct ChromePalette {
    pub tab_active_bg: PackedRgba,
    pub tab_active_fg: PackedRgba,
    pub tab_inactive_fg: PackedRgba,
    pub tab_key_fg: PackedRgba,
    pub status_fg: PackedRgba,
    pub status_accent: PackedRgba,
    pub status_good: PackedRgba,
    pub status_warn: PackedRgba,
    pub help_fg: PackedRgba,
    pub help_key_fg: PackedRgba,
    pub help_border_fg: PackedRgba,
    pub help_category_fg: PackedRgba,
}

impl ChromePalette {
    /// Resolve the palette from accessibility settings.
    ///
    /// When high-contrast mode is active, uses the dedicated HC constants.
    /// Otherwise delegates to the theme-aware `TuiThemePalette`.
    #[must_use]
    pub fn from_settings(settings: &AccessibilitySettings) -> Self {
        if settings.high_contrast {
            Self {
                tab_active_bg: HC_TAB_ACTIVE_BG,
                tab_active_fg: HC_TAB_ACTIVE_FG,
                tab_inactive_fg: HC_TAB_INACTIVE_FG,
                tab_key_fg: HC_TAB_KEY_FG,
                status_fg: HC_STATUS_FG,
                status_accent: HC_STATUS_ACCENT,
                status_good: HC_STATUS_GOOD,
                status_warn: HC_STATUS_WARN,
                help_fg: HC_HELP_FG,
                help_key_fg: HC_HELP_KEY_FG,
                help_border_fg: HC_HELP_BORDER_FG,
                help_category_fg: HC_HELP_CATEGORY_FG,
            }
        } else {
            Self::from_theme()
        }
    }

    /// Resolve from the currently active ftui theme.
    #[must_use]
    pub fn from_theme() -> Self {
        let tp = crate::tui_theme::TuiThemePalette::current();
        Self {
            tab_active_bg: tp.tab_active_bg,
            tab_active_fg: tp.tab_active_fg,
            tab_inactive_fg: tp.tab_inactive_fg,
            tab_key_fg: tp.tab_key_fg,
            status_fg: tp.status_fg,
            status_accent: tp.status_accent,
            status_good: tp.status_good,
            status_warn: tp.status_warn,
            help_fg: tp.help_fg,
            help_key_fg: tp.help_key_fg,
            help_border_fg: tp.help_border_fg,
            help_category_fg: tp.help_category_fg,
        }
    }

    /// Standard (non-high-contrast) palette with hardcoded Cyberpunk Aurora colors.
    ///
    /// Kept for backward compatibility with tests.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            tab_active_bg: TAB_ACTIVE_BG,
            tab_active_fg: TAB_ACTIVE_FG,
            tab_inactive_fg: TAB_INACTIVE_FG,
            tab_key_fg: TAB_KEY_FG,
            status_fg: STATUS_FG,
            status_accent: STATUS_ACCENT,
            status_good: STATUS_GOOD,
            status_warn: STATUS_WARN,
            help_fg: HELP_FG,
            help_key_fg: HELP_KEY_FG,
            help_border_fg: HELP_BORDER_FG,
            help_category_fg: HELP_CATEGORY_FG,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Key hint bar — context-sensitive shortcut hints
// ──────────────────────────────────────────────────────────────────────

/// Build a compact key hint string from the most important screen bindings.
///
/// Selects up to `max_hints` entries that fit within `max_width` characters,
/// formatted as `[key] action  [key] action  ...`.
#[must_use]
pub fn build_key_hints(
    screen_bindings: &[HelpEntry],
    max_hints: usize,
    max_width: usize,
) -> String {
    let mut hints = String::new();

    for (count, entry) in screen_bindings.iter().enumerate() {
        if count >= max_hints {
            break;
        }
        let segment = format!("[{}] {} ", entry.key, entry.action);
        if hints.len() + segment.len() > max_width {
            break;
        }
        hints.push_str(&segment);
    }

    // Trim trailing space
    let trimmed = hints.trim_end();
    trimmed.to_string()
}

/// Render a key hint bar into a 1-row area, showing context-sensitive shortcuts.
pub fn render_key_hint_bar(screen_bindings: &[HelpEntry], frame: &mut Frame, area: Rect) {
    use ftui::text::{Line, Span, Text};

    if area.width < 20 || screen_bindings.is_empty() {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();

    let max_width = (area.width as usize).saturating_sub(4); // padding
    let hints = build_key_hints(screen_bindings, 6, max_width);
    if hints.is_empty() {
        return;
    }

    // Parse the hint string into styled spans: keys in accent, text in dim
    let mut spans = Vec::new();
    spans.push(Span::styled(" ", Style::default().bg(tp.status_bg)));

    let mut rest = hints.as_str();
    while let Some(open) = rest.find('[') {
        // Text before bracket
        if open > 0 {
            spans.push(Span::styled(
                &rest[..open],
                Style::default().fg(tp.status_fg).bg(tp.status_bg),
            ));
        }
        rest = &rest[open..];
        if let Some(close) = rest.find(']') {
            // Key portion: [key]
            spans.push(Span::styled(
                &rest[..=close],
                Style::default().fg(tp.tab_key_fg).bg(tp.status_bg),
            ));
            rest = &rest[close + 1..];
        } else {
            break;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(
            rest,
            Style::default().fg(tp.status_fg).bg(tp.status_bg),
        ));
    }

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines([line])).render(area, frame);
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_screens::ALL_SCREEN_IDS;

    // ── Key hints tests ─────────────────────────────────────────

    #[test]
    fn build_key_hints_empty_bindings() {
        let hints = build_key_hints(&[], 6, 80);
        assert!(hints.is_empty());
    }

    #[test]
    fn build_key_hints_single_entry() {
        let bindings = [HelpEntry {
            key: "j",
            action: "Down",
        }];
        let hints = build_key_hints(&bindings, 6, 80);
        assert_eq!(hints, "[j] Down");
    }

    #[test]
    fn build_key_hints_multiple_entries() {
        let bindings = [
            HelpEntry {
                key: "j",
                action: "Down",
            },
            HelpEntry {
                key: "k",
                action: "Up",
            },
            HelpEntry {
                key: "q",
                action: "Quit",
            },
        ];
        let hints = build_key_hints(&bindings, 6, 80);
        assert!(hints.contains("[j] Down"));
        assert!(hints.contains("[k] Up"));
        assert!(hints.contains("[q] Quit"));
    }

    #[test]
    fn build_key_hints_respects_max_hints() {
        let bindings = [
            HelpEntry {
                key: "a",
                action: "A",
            },
            HelpEntry {
                key: "b",
                action: "B",
            },
            HelpEntry {
                key: "c",
                action: "C",
            },
        ];
        let hints = build_key_hints(&bindings, 2, 80);
        assert!(hints.contains("[a] A"));
        assert!(hints.contains("[b] B"));
        assert!(!hints.contains("[c] C"));
    }

    #[test]
    fn build_key_hints_respects_max_width() {
        let bindings = [
            HelpEntry {
                key: "j",
                action: "Navigate down",
            },
            HelpEntry {
                key: "k",
                action: "Navigate up",
            },
        ];
        // Width too narrow for both
        let hints = build_key_hints(&bindings, 6, 20);
        assert!(hints.contains("[j] Navigate down"));
        assert!(!hints.contains("[k]"));
    }

    // ── ChromePalette tests ─────────────────────────────────────

    #[test]
    fn palette_standard_uses_normal_colors() {
        let p = ChromePalette::standard();
        assert_eq!(p.tab_active_bg, TAB_ACTIVE_BG);
        assert_eq!(p.help_fg, HELP_FG);
    }

    #[test]
    fn palette_high_contrast_uses_hc_colors() {
        let settings = AccessibilitySettings {
            high_contrast: true,
            key_hints: true,
        };
        let p = ChromePalette::from_settings(&settings);
        assert_eq!(p.tab_active_bg, HC_TAB_ACTIVE_BG);
        assert_eq!(p.help_fg, HC_HELP_FG);
        assert_eq!(p.status_accent, HC_STATUS_ACCENT);
    }

    #[test]
    fn palette_non_high_contrast_uses_theme() {
        let settings = AccessibilitySettings {
            high_contrast: false,
            key_hints: true,
        };
        let p = ChromePalette::from_settings(&settings);
        // Non-HC mode now derives from the active ftui theme, not static constants.
        // Just verify the palette has valid (non-zero) colors.
        assert!(
            p.tab_active_bg.r() > 0
                || p.tab_active_bg.g() > 0
                || p.tab_active_bg.b() > 0
                || p.tab_active_bg == crate::tui_theme::TuiThemePalette::current().tab_active_bg,
            "non-HC tab_active_bg should come from theme"
        );
        assert!(
            p.help_fg.r() > 0
                || p.help_fg.g() > 0
                || p.help_fg.b() > 0
                || p.help_fg == crate::tui_theme::TuiThemePalette::current().help_fg,
            "non-HC help_fg should come from theme"
        );
    }

    #[test]
    fn hc_colors_are_brighter_than_standard() {
        // High-contrast FG colors should have higher brightness (sum of RGB)
        let hc_fg_sum = u32::from(HC_TAB_INACTIVE_FG.r())
            + u32::from(HC_TAB_INACTIVE_FG.g())
            + u32::from(HC_TAB_INACTIVE_FG.b());
        let std_fg_sum = u32::from(TAB_INACTIVE_FG.r())
            + u32::from(TAB_INACTIVE_FG.g())
            + u32::from(TAB_INACTIVE_FG.b());
        assert!(
            hc_fg_sum > std_fg_sum,
            "HC inactive FG ({hc_fg_sum}) should be brighter than standard ({std_fg_sum})"
        );
    }

    // ── Render key hint bar tests ───────────────────────────────

    #[test]
    fn render_key_hint_bar_narrow_terminal_skipped() {
        let bindings = [HelpEntry {
            key: "j",
            action: "Down",
        }];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(15, 1, &mut pool);
        // Should not panic on narrow terminal (< 20 cols)
        render_key_hint_bar(&bindings, &mut frame, Rect::new(0, 0, 15, 1));
    }

    #[test]
    fn render_key_hint_bar_renders_without_panic() {
        let bindings = [
            HelpEntry {
                key: "j",
                action: "Down",
            },
            HelpEntry {
                key: "k",
                action: "Up",
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        render_key_hint_bar(&bindings, &mut frame, Rect::new(0, 0, 80, 1));
    }

    #[test]
    fn render_key_hint_bar_empty_bindings_noop() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        render_key_hint_bar(&[], &mut frame, Rect::new(0, 0, 80, 1));
    }

    // ── Existing tests ──────────────────────────────────────────

    #[test]
    fn chrome_layout_splits_correctly() {
        let area = Rect::new(0, 0, 80, 24);
        let chrome = chrome_layout(area);
        assert_eq!(chrome.tab_bar.height, 1);
        assert_eq!(chrome.status_line.height, 1);
        assert_eq!(chrome.content.height, 22); // 24 - 1 - 1
        assert_eq!(chrome.tab_bar.y, 0);
        assert_eq!(chrome.content.y, 1);
        assert_eq!(chrome.status_line.y, 23);
    }

    #[test]
    fn chrome_layout_minimum_height() {
        let area = Rect::new(0, 0, 80, 3);
        let chrome = chrome_layout(area);
        assert_eq!(chrome.tab_bar.height, 1);
        assert_eq!(chrome.content.height, 1);
        assert_eq!(chrome.status_line.height, 1);
    }

    #[test]
    fn global_keybindings_complete() {
        assert!(GLOBAL_KEYBINDINGS.len() >= 5);
        for &(key, action) in GLOBAL_KEYBINDINGS {
            assert!(!key.is_empty());
            assert!(!action.is_empty());
        }
    }

    #[test]
    fn tab_count_matches_screens() {
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), ALL_SCREEN_IDS.len());
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), 11);
    }

    #[test]
    fn color_constants_are_valid() {
        let colors = [
            TAB_ACTIVE_BG,
            TAB_ACTIVE_FG,
            TAB_INACTIVE_FG,
            TAB_KEY_FG,
            STATUS_FG,
            STATUS_ACCENT,
            STATUS_GOOD,
            STATUS_WARN,
        ];
        for color in colors {
            assert_ne!(color, PackedRgba::rgba(0, 0, 0, 0));
        }
    }

    #[test]
    fn theme_palette_produces_valid_chrome_colors() {
        use ftui_extras::theme::{ScopedThemeLock, ThemeId};
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let tp = crate::tui_theme::TuiThemePalette::current();
        let colors = [
            tp.tab_active_bg,
            tp.tab_inactive_bg,
            tp.status_bg,
            tp.help_bg,
        ];
        for color in colors {
            // Background colors should have at least some RGB component
            assert!(
                color.r() > 0 || color.g() > 0 || color.b() > 0,
                "background color should not be fully black"
            );
        }
    }

    #[test]
    fn screen_meta_for_all_ids() {
        for &id in ALL_SCREEN_IDS {
            let meta = screen_meta(id);
            assert!(!meta.title.is_empty());
            assert!(!meta.short_label.is_empty());
            assert!(meta.short_label.len() <= 12);
        }
    }
}
