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

// High-contrast palette is now handled at the theme level
// (`ThemeId::HighContrast` in `TuiThemePalette`). Legacy HC_*
// constants removed.

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

/// Map a screen category to a theme color for the tab key indicator.
fn category_key_color(
    category: crate::tui_screens::ScreenCategory,
    tp: &crate::tui_theme::TuiThemePalette,
) -> PackedRgba {
    use crate::tui_screens::ScreenCategory;
    match category {
        ScreenCategory::Overview => tp.status_accent,
        ScreenCategory::Communication => tp.metric_messages,
        ScreenCategory::Operations => tp.severity_warn,
        ScreenCategory::System => tp.severity_ok,
    }
}

/// Render the tab bar into a 1-row area.
pub fn render_tab_bar(active: MailScreenId, effects_enabled: bool, frame: &mut Frame, area: Rect) {
    use ftui::text::{Line, Span, Text};
    use ftui_extras::text_effects::{ColorGradient, StyledText, TextEffect};

    let tp = crate::tui_theme::TuiThemePalette::current();

    // Fill background
    let bg_style = Style::default().bg(tp.tab_inactive_bg);
    Paragraph::new("").style(bg_style).render(area, frame);

    let mut x = area.x;
    let available = area.width;

    // Determine width mode:
    // - Ultra-compact (< 40): key only, no label
    // - Compact (< 60): short labels
    // - Normal (>= 60): full titles
    let ultra_compact = available < 40;
    let compact = available < 60;

    // Track previous category for inter-category separator.
    let mut prev_category: Option<crate::tui_screens::ScreenCategory> = None;

    for (i, meta) in MAIL_SCREEN_REGISTRY.iter().enumerate() {
        let number = i + 1;
        let label = if ultra_compact {
            "" // Key number only
        } else if compact {
            meta.short_label
        } else {
            meta.title
        };
        let is_active = meta.id == active;
        let category_changed = prev_category.map_or(i > 0, |c| c != meta.category);

        // " 1:Label " — each tab has fixed structure
        let key_str = format!("{number}");
        // Width: indicator + space + key + colon? + label? + space
        let has_label = !label.is_empty();
        let tab_width = if has_label {
            u16::try_from(1 + key_str.len() + 1 + label.len() + 1).unwrap_or(u16::MAX)
        } else {
            // Ultra-compact: " 1 " (space + key + space)
            u16::try_from(1 + key_str.len() + 1).unwrap_or(u16::MAX)
        };

        // Inter-tab separator (heavier between categories, lighter within)
        if i > 0 && x < area.x + available {
            let (sep_char, sep_fg) = if category_changed {
                // Wider gap between categories: dim separator
                ("┃", tp.text_muted)
            } else {
                ("│", tp.tab_inactive_fg)
            };
            let sep_area = Rect::new(x, area.y, 1, 1);
            Paragraph::new(sep_char)
                .style(Style::default().fg(sep_fg).bg(tp.tab_inactive_bg))
                .render(sep_area, frame);
            x += 1;
        }

        if x + tab_width > area.x + available {
            break; // Don't overflow
        }

        let (fg, bg) = if is_active {
            (tp.tab_active_fg, tp.tab_active_bg)
        } else {
            (tp.tab_inactive_fg, tp.tab_inactive_bg)
        };

        let tab_area = Rect::new(x, area.y, tab_width, 1);

        let use_gradient = is_active && effects_enabled;
        let label_style = if is_active {
            Style::default().fg(fg).bg(bg).bold()
        } else {
            Style::default().fg(fg).bg(bg)
        };

        let label_span = if use_gradient && has_label {
            // Reserve label width in the base tab row; overlay gradient text below.
            Span::styled(" ".repeat(label.len()), Style::default().bg(bg))
        } else if has_label {
            Span::styled(label, label_style)
        } else {
            Span::styled("", Style::default())
        };

        // Use category-specific color for the key number to aid wayfinding.
        let key_fg = category_key_color(meta.category, &tp);

        // Active tab indicator: bold key with underline for strong contrast.
        let key_style = if is_active {
            Style::default().fg(key_fg).bg(bg).bold().underline()
        } else {
            Style::default().fg(key_fg).bg(bg)
        };

        let mut spans = vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(key_str.as_str(), key_style),
        ];
        if has_label {
            spans.push(Span::styled(":", Style::default().fg(tp.tab_inactive_fg).bg(bg)));
            spans.push(label_span);
        }
        spans.push(Span::styled(" ", Style::default().bg(bg)));

        Paragraph::new(Text::from_lines([Line::from_spans(spans)])).render(tab_area, frame);

        if use_gradient && has_label {
            let gradient =
                ColorGradient::new(vec![(0.0, tp.status_accent), (1.0, tp.text_secondary)]);
            let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
            let key_width = u16::try_from(key_str.len()).unwrap_or(u16::MAX);
            let label_x = x + 1 + key_width + 1;
            StyledText::new(label)
                .effect(TextEffect::HorizontalGradient { gradient })
                .base_color(tp.status_accent)
                .bold()
                .render(Rect::new(label_x, area.y, label_width, 1), frame);
        }

        prev_category = Some(meta.category);
        x += tab_width;
    }
}

/// Compute and record per-tab hit slots into the mouse dispatcher.
///
/// This mirrors the tab-width logic from [`render_tab_bar`] so that
/// mouse click coordinates can be mapped back to the correct screen.
pub fn record_tab_hit_slots(
    area: Rect,
    dispatcher: &crate::tui_hit_regions::MouseDispatcher,
) {
    let available = area.width;
    let ultra_compact = available < 40;
    let compact = available < 60;
    let mut x = area.x;

    for (i, meta) in MAIL_SCREEN_REGISTRY.iter().enumerate() {
        let number = i + 1;
        let label = if ultra_compact {
            ""
        } else if compact {
            meta.short_label
        } else {
            meta.title
        };
        let key_str_len = if number >= 10 { 2 } else { 1 };
        let has_label = !label.is_empty();
        #[allow(clippy::cast_possible_truncation)]
        let tab_width: u16 = if has_label {
            (1 + key_str_len + 1 + label.len() + 1) as u16
        } else {
            (1 + key_str_len + 1) as u16
        };

        // Separator before each tab except the first.
        if i > 0 && x < area.x + available {
            x += 1;
        }

        if x + tab_width > area.x + available {
            break;
        }

        dispatcher.record_tab_slot(i, meta.id, x, x + tab_width, area.y);
        x += tab_width;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Status line
// ──────────────────────────────────────────────────────────────────────

/// Semantic priority level for status-bar segments.
///
/// Segments are added in priority order; lower-priority segments are
/// the first to be dropped when the terminal is too narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StatusPriority {
    /// Always shown (screen name, help hint).
    Critical = 0,
    /// Shown at >= 60 cols (transport mode).
    High = 1,
    /// Shown at >= 80 cols (uptime, error count).
    Medium = 2,
    /// Shown at >= 100 cols (full counters, latency, key hints).
    Low = 3,
}

/// A semantic segment of the status bar.
struct StatusSegment {
    priority: StatusPriority,
    text: String,
    fg: PackedRgba,
    bold: bool,
}

/// Compute which segments to show given available width.
///
/// Segments are grouped into left (always left-aligned), center
/// (centered between left and right), and right (right-aligned).
/// Lower-priority segments are dropped until everything fits.
#[allow(clippy::too_many_lines)]
fn plan_status_segments(
    state: &TuiSharedState,
    active: MailScreenId,
    help_visible: bool,
    accessibility: &AccessibilitySettings,
    screen_bindings: &[HelpEntry],
    toast_muted: bool,
    available: u16,
) -> (Vec<StatusSegment>, Vec<StatusSegment>, Vec<StatusSegment>) {
    let counters = state.request_counters();
    let uptime = state.uptime();
    let meta = screen_meta(active);
    let transport_mode = state.config_snapshot().transport_mode();
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Uptime formatting
    let uptime_secs = uptime.as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;
    let secs = uptime_secs % 60;
    let uptime_str = if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m{secs:02}s")
    };

    // Counter data
    let avg_latency = state.avg_latency_ms();
    let error_count = counters.status_4xx + counters.status_5xx;
    let total = counters.total;
    let ok = counters.status_2xx;
    let counter_fg = if error_count > 0 { tp.status_warn } else { tp.status_good };

    // ── Left segments (always left-aligned) ──
    let mut left = vec![StatusSegment {
        priority: StatusPriority::Critical,
        text: format!(" {}", meta.title),
        fg: tp.status_accent,
        bold: true,
    }];

    // Transport mode (High priority)
    left.push(StatusSegment {
        priority: StatusPriority::High,
        text: format!(" {transport_mode}"),
        fg: tp.status_fg,
        bold: false,
    });

    // Uptime (Medium priority)
    left.push(StatusSegment {
        priority: StatusPriority::Medium,
        text: format!(" up:{uptime_str}"),
        fg: tp.status_fg,
        bold: false,
    });

    // ── Center segments (centered) ──
    let mut center = Vec::new();

    // Error count alone at Medium priority (most critical counter)
    if error_count > 0 {
        center.push(StatusSegment {
            priority: StatusPriority::Medium,
            text: format!("err:{error_count}"),
            fg: tp.status_warn,
            bold: true,
        });
    }

    // Full counter string at Low priority
    center.push(StatusSegment {
        priority: StatusPriority::Low,
        text: format!("req:{total} ok:{ok} err:{error_count} avg:{avg_latency}ms"),
        fg: counter_fg,
        bold: false,
    });

    // Key hints at Low priority
    if accessibility.key_hints && !accessibility.screen_reader && !screen_bindings.is_empty() {
        let max_hint = (available / 3).max(20) as usize;
        let hints = build_key_hints(screen_bindings, 6, max_hint);
        if !hints.is_empty() {
            center.push(StatusSegment {
                priority: StatusPriority::Low,
                text: hints,
                fg: tp.status_fg,
                bold: false,
            });
        }
    }

    // ── Right segments (right-aligned) ──
    let help_hint = if help_visible { "[?]" } else { "?" };
    let mut right = vec![StatusSegment {
        priority: StatusPriority::Critical,
        text: format!("{help_hint} "),
        fg: tp.tab_key_fg,
        bold: false,
    }];

    // Toast mute indicator (High priority)
    if toast_muted {
        right.push(StatusSegment {
            priority: StatusPriority::High,
            text: "[muted] ".to_string(),
            fg: tp.status_warn,
            bold: false,
        });
    }

    // Accessibility indicators (Medium priority)
    let a11y = match (accessibility.reduced_motion, accessibility.screen_reader) {
        (false, false) => None,
        (true, false) => Some("[rm]"),
        (false, true) => Some("[sr]"),
        (true, true) => Some("[rm,sr]"),
    };
    if let Some(hint) = a11y {
        right.push(StatusSegment {
            priority: StatusPriority::Medium,
            text: format!("{hint} "),
            fg: tp.status_fg,
            bold: false,
        });
    }

    // Drop segments that don't fit.
    // Width breakpoints: Critical always fits, High >= 60, Medium >= 80, Low >= 100.
    let max_priority = if available >= 100 {
        StatusPriority::Low
    } else if available >= 80 {
        StatusPriority::Medium
    } else if available >= 60 {
        StatusPriority::High
    } else {
        StatusPriority::Critical
    };

    left.retain(|s| s.priority <= max_priority);
    center.retain(|s| s.priority <= max_priority);
    right.retain(|s| s.priority <= max_priority);

    // If both Medium error count and Low full counters survived, drop
    // the Medium error-only duplicate (full counters include it).
    if center.len() > 1
        && center.iter().any(|s| s.priority == StatusPriority::Low && s.text.contains("req:"))
    {
        center.retain(|s| !(s.priority == StatusPriority::Medium && s.text.starts_with("err:")));
    }

    (left, center, right)
}

/// Render the status line into a 1-row area.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn render_status_line(
    state: &TuiSharedState,
    active: MailScreenId,
    help_visible: bool,
    accessibility: &AccessibilitySettings,
    screen_bindings: &[HelpEntry],
    toast_muted: bool,
    frame: &mut Frame,
    area: Rect,
) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();

    // Fill background
    let bg_style = Style::default().bg(tp.status_bg);
    Paragraph::new("").style(bg_style).render(area, frame);

    let (left, center, right) = plan_status_segments(
        state,
        active,
        help_visible,
        accessibility,
        screen_bindings,
        toast_muted,
        area.width,
    );

    // Compute total widths.
    let left_width: u16 = left.iter().map(|s| s.text.len() as u16).sum();
    let center_width: u16 = center
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let sep = if i > 0 { 3u16 } else { 0 }; // " | "
            s.text.len() as u16 + sep
        })
        .sum();
    let right_width: u16 = right.iter().map(|s| s.text.len() as u16).sum();

    let mut spans: Vec<Span<'_>> = Vec::with_capacity(16);

    // Left segments
    for seg in &left {
        let mut style = Style::default().fg(seg.fg).bg(tp.status_bg);
        if seg.bold {
            style = style.bold();
        }
        spans.push(Span::styled(seg.text.as_str(), style));
    }

    // Center padding + center segments
    let total_fixed = left_width + center_width + right_width;
    if center_width > 0 && total_fixed < area.width {
        let gap = area.width - total_fixed;
        let left_pad = gap / 2;
        if left_pad > 0 {
            spans.push(Span::styled(
                " ".repeat(left_pad as usize),
                bg_style,
            ));
        }
    } else if center_width == 0 {
        // No center — push right to the far right.
        let gap = area.width.saturating_sub(left_width + right_width);
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap as usize), bg_style));
        }
    }

    for (i, seg) in center.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " | ",
                Style::default().fg(tp.status_fg).bg(tp.status_bg),
            ));
        }
        // Key hints get keycap/chip rendering (reverse-video keys).
        if seg.priority == StatusPriority::Low && seg.text.contains('\x01') {
            push_keycap_chip_spans(&mut spans, &seg.text, &tp);
        } else {
            let mut style = Style::default().fg(seg.fg).bg(tp.status_bg);
            if seg.bold {
                style = style.bold();
            }
            spans.push(Span::styled(seg.text.as_str(), style));
        }
    }

    // Right padding
    if center_width > 0 && total_fixed < area.width {
        let gap = area.width - total_fixed;
        let right_pad = gap - gap / 2;
        if right_pad > 0 {
            spans.push(Span::styled(
                " ".repeat(right_pad as usize),
                bg_style,
            ));
        }
    }

    // Right segments
    for seg in &right {
        let mut style = Style::default().fg(seg.fg).bg(tp.status_bg);
        if seg.bold {
            style = style.bold();
        }
        spans.push(Span::styled(seg.text.as_str(), style));
    }

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines([line])).render(area, frame);
}

// ──────────────────────────────────────────────────────────────────────
// Help overlay
// ──────────────────────────────────────────────────────────────────────

/// Build the global keybindings list with a registry-derived jump-key legend.
fn global_keybindings() -> Vec<(String, &'static str)> {
    vec![
        (crate::tui_screens::jump_key_legend(), "Jump to screen"),
        ("Tab".to_string(), "Next screen"),
        ("Shift+Tab".to_string(), "Previous screen"),
        ("m".to_string(), "Toggle MCP/API mode"),
        ("Ctrl+P / :".to_string(), "Command palette"),
        ("T".to_string(), "Cycle theme"),
        ("?".to_string(), "Toggle help"),
        ("q".to_string(), "Quit"),
        ("Esc".to_string(), "Dismiss overlay"),
    ]
}

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
        .border_type(BorderType::Rounded)
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
    let global_bindings = global_keybindings();
    for (key, action) in &global_bindings {
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

/// Render the help overlay from structured `HelpSection`s (profile-aware).
///
/// This version displays the profile name in the title and supports
/// scrolling through sections with a scroll offset.
pub fn render_help_overlay_sections(
    sections: &[crate::tui_keymap::HelpSection],
    scroll_offset: u16,
    frame: &mut Frame,
    area: Rect,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();

    let overlay_width = (u32::from(area.width) * 60 / 100).clamp(36, 72) as u16;
    let overlay_height = (u32::from(area.height) * 60 / 100).clamp(10, 28) as u16;
    let overlay_width = overlay_width.min(area.width.saturating_sub(2));
    let overlay_height = overlay_height.min(area.height.saturating_sub(2));

    let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

    // Total line count for scroll indicator.
    let total_lines: usize = sections
        .iter()
        .map(|s| s.line_count() + 1) // +1 for blank separator between sections
        .sum::<usize>()
        .saturating_sub(1); // no trailing separator

    let scroll_hint = if total_lines > usize::from(overlay_height.saturating_sub(2)) {
        " (j/k to scroll) "
    } else {
        " "
    };

    let title = format!(" Keyboard Shortcuts{scroll_hint}(Esc to close) ");
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.as_str())
        .style(Style::default().fg(tp.help_border_fg).bg(tp.help_bg));

    let inner = block.inner(overlay_area);
    block.render(overlay_area, frame);

    let key_col = 14u16;
    let col_width = inner.width.saturating_sub(1);
    let mut line_idx: u16 = 0;
    let visible_start = scroll_offset;
    let visible_end = scroll_offset.saturating_add(inner.height);
    let mut y_pos = 0u16;

    for (si, section) in sections.iter().enumerate() {
        // Blank separator between sections (except before the first).
        if si > 0 {
            line_idx += 1;
        }

        // Section header.
        if line_idx >= visible_start && line_idx < visible_end && y_pos < inner.height {
            let header = Paragraph::new(section.title.as_str()).style(
                Style::default()
                    .fg(tp.help_category_fg)
                    .bg(tp.help_bg)
                    .bold(),
            );
            header.render(Rect::new(inner.x + 1, inner.y + y_pos, col_width, 1), frame);
            y_pos += 1;
        }
        line_idx += 1;

        // Entries.
        for (key, action) in &section.entries {
            if line_idx >= visible_start && line_idx < visible_end && y_pos < inner.height {
                render_keybinding_line_themed(
                    key,
                    action,
                    Rect::new(inner.x + 1, inner.y + y_pos, col_width, 1),
                    key_col,
                    &tp,
                    frame,
                );
                y_pos += 1;
            }
            line_idx += 1;
        }
    }
}

/// Render a single keybinding line with keycap style: `  key   action`.
///
/// The key is rendered in reverse-video (keycap style) with the action
/// label right-padded to align with `key_col`.
fn render_keybinding_line_themed(
    key: &str,
    action: &str,
    area: Rect,
    key_col: u16,
    tp: &crate::tui_theme::TuiThemePalette,
    frame: &mut Frame,
) {
    use ftui::text::{Line, Span, Text};

    let keycap = format!(" {key} ");
    // Total width of leading space + keycap
    let keycap_total = 2 + keycap.len(); // "  " prefix + keycap
    let key_len = u16::try_from(keycap_total).unwrap_or(key_col);
    let pad_len = key_col.saturating_sub(key_len) as usize;
    let padding = " ".repeat(pad_len);

    let keycap_style = Style::default()
        .fg(tp.help_bg)
        .bg(tp.help_key_fg)
        .bold();

    let spans = vec![
        Span::styled("  ", Style::default().bg(tp.help_bg)),
        Span::styled(keycap, keycap_style),
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
    /// Always delegates to the theme-aware `TuiThemePalette::current()`.
    /// High-contrast mode is handled at the theme level (`ThemeId::HighContrast`).
    #[must_use]
    pub fn from_settings(_settings: &AccessibilitySettings) -> Self {
        Self::from_theme()
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
// Key hint bar — keycap/action-chip style shortcut hints
// ──────────────────────────────────────────────────────────────────────

/// Width of a single keycap/action chip: ` key ` + ` action` + separator.
///
/// Returns the display width (key padded + action + trailing separator).
fn chip_width(key: &str, action: &str, is_last: bool) -> usize {
    // ` key ` (reverse-video keycap) + ` action` + ` · ` separator (3 if not last)
    let keycap = key.len() + 2; // space + key + space
    let act = 1 + action.len(); // space + action
    let sep = if is_last { 0 } else { 3 }; // " · "
    keycap + act + sep
}

/// Build a compact key hint string from the most important screen bindings.
///
/// Selects up to `max_hints` entries that fit within `max_width` characters,
/// formatted as keycap/action chips: ` key  action · key  action`.
#[must_use]
pub fn build_key_hints(
    screen_bindings: &[HelpEntry],
    max_hints: usize,
    max_width: usize,
) -> String {
    let mut hints = String::new();
    let mut used = 0usize;
    let count = screen_bindings.len().min(max_hints);

    for (i, entry) in screen_bindings.iter().take(count).enumerate() {
        let is_last = i + 1 >= count
            || used + chip_width(entry.key, entry.action, false)
                + chip_width(
                    screen_bindings.get(i + 1).map_or("", |e| e.key),
                    screen_bindings.get(i + 1).map_or("", |e| e.action),
                    true,
                )
                > max_width;

        let w = chip_width(entry.key, entry.action, is_last);
        if used + w > max_width {
            break;
        }

        if !hints.is_empty() {
            hints.push_str(" \u{00b7} "); // " · "
        }
        // Keycap markers — rendered as reverse-video by the span builder.
        hints.push('\x01'); // SOH marks keycap start
        hints.push_str(entry.key);
        hints.push('\x02'); // STX marks keycap end
        hints.push(' ');
        hints.push_str(entry.action);
        used += w;

        if is_last {
            break;
        }
    }

    hints
}

/// Push keycap/action-chip styled spans parsed from a `build_key_hints` string.
///
/// Keycap regions are delimited by `\x01..\x02` and rendered in reverse-video;
/// action text is rendered in dim/normal style; separators (`·`) in dim.
fn push_keycap_chip_spans<'a>(
    spans: &mut Vec<ftui::text::Span<'a>>,
    hints: &'a str,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    use ftui::text::Span;

    let keycap_style = Style::default()
        .fg(tp.status_bg)
        .bg(tp.tab_key_fg)
        .bold();
    let action_style = Style::default().fg(tp.status_fg).bg(tp.status_bg);
    let sep_style = Style::default()
        .fg(tp.tab_inactive_fg)
        .bg(tp.status_bg);

    let mut rest = hints;
    while !rest.is_empty() {
        if let Some(start) = rest.find('\x01') {
            // Text before keycap (separator or leading space)
            if start > 0 {
                spans.push(Span::styled(&rest[..start], sep_style));
            }
            rest = &rest[start + 1..]; // skip SOH
            if let Some(end) = rest.find('\x02') {
                // Keycap: ` key ` in reverse-video
                let key = &rest[..end];
                spans.push(Span::styled(
                    format!(" {key} "),
                    keycap_style,
                ));
                rest = &rest[end + 1..]; // skip STX
            } else {
                // Malformed — dump remaining
                spans.push(Span::styled(rest, action_style));
                break;
            }
        } else {
            // No more keycaps — remaining is action text / separators
            spans.push(Span::styled(rest, action_style));
            break;
        }
    }
}

/// Render a key hint bar into a 1-row area using keycap/action-chip style.
///
/// Each binding is rendered as a reverse-video keycap followed by its
/// action label, separated by middle-dot (`·`) dividers.
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

    let mut spans = Vec::new();
    spans.push(Span::styled(" ", Style::default().bg(tp.status_bg)));
    push_keycap_chip_spans(&mut spans, &hints, &tp);

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
        // Keycap markers: \x01 key \x02 action
        assert!(hints.contains("\x01j\x02"));
        assert!(hints.contains("Down"));
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
        assert!(hints.contains("\x01j\x02 Down"));
        assert!(hints.contains("\x01k\x02 Up"));
        assert!(hints.contains("\x01q\x02 Quit"));
        // Chips separated by middle dot
        assert!(hints.contains(" \u{00b7} "));
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
        assert!(hints.contains("\x01a\x02 A"));
        assert!(hints.contains("\x01b\x02 B"));
        assert!(!hints.contains("\x01c\x02"));
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
        // Width too narrow for both chips
        let hints = build_key_hints(&bindings, 6, 20);
        assert!(hints.contains("\x01j\x02 Navigate down"));
        assert!(!hints.contains("\x01k\x02"));
    }

    #[test]
    fn keycap_chip_spans_produce_reverse_video_keys() {
        use ftui::text::Span;
        let tp = crate::tui_theme::TuiThemePalette::current();
        let hints = build_key_hints(
            &[
                HelpEntry { key: "j", action: "Down" },
                HelpEntry { key: "k", action: "Up" },
            ],
            6,
            80,
        );
        let mut spans: Vec<Span<'_>> = Vec::new();
        push_keycap_chip_spans(&mut spans, &hints, &tp);
        // Should produce at least keycap + action spans for each chip
        assert!(spans.len() >= 4, "expected >= 4 spans, got {}", spans.len());
        // First keycap span should be bold (reverse-video keycap)
        let first_keycap = &spans[0];
        let attrs = first_keycap.style.and_then(|s| s.attrs).unwrap_or(ftui::style::StyleFlags::NONE);
        assert!(
            attrs.contains(ftui::style::StyleFlags::BOLD),
            "keycap span should be bold"
        );
    }

    // ── ChromePalette tests ─────────────────────────────────────

    #[test]
    fn palette_standard_uses_normal_colors() {
        let p = ChromePalette::standard();
        assert_eq!(p.tab_active_bg, TAB_ACTIVE_BG);
        assert_eq!(p.help_fg, HELP_FG);
    }

    #[test]
    fn palette_high_contrast_resolves_from_theme() {
        let settings = AccessibilitySettings {
            high_contrast: true,
            key_hints: true,
            reduced_motion: false,
            screen_reader: false,
        };
        let p = ChromePalette::from_settings(&settings);
        let tp = crate::tui_theme::TuiThemePalette::current();
        assert_eq!(p.tab_active_bg, tp.tab_active_bg);
        assert_eq!(p.help_fg, tp.help_fg);
        assert_eq!(p.status_accent, tp.status_accent);
    }

    #[test]
    fn palette_non_high_contrast_uses_theme() {
        let settings = AccessibilitySettings {
            high_contrast: false,
            key_hints: true,
            reduced_motion: false,
            screen_reader: false,
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
    fn standard_palette_has_non_zero_colors() {
        let p = ChromePalette::standard();
        // Verify the standard palette constants are non-trivial
        let fg_sum = u32::from(p.tab_inactive_fg.r())
            + u32::from(p.tab_inactive_fg.g())
            + u32::from(p.tab_inactive_fg.b());
        assert!(fg_sum > 0, "standard inactive FG should be non-zero");
        assert_ne!(
            p.tab_active_bg, p.tab_active_fg,
            "active BG and FG should differ"
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
        let bindings = global_keybindings();
        assert!(bindings.len() >= 5);
        for (key, action) in &bindings {
            assert!(!key.is_empty(), "empty key in global keybindings");
            assert!(!action.is_empty(), "empty action in global keybindings");
        }
    }

    #[test]
    fn global_keybindings_jump_key_matches_registry() {
        let bindings = global_keybindings();
        let jump_entry = &bindings[0];
        assert_eq!(jump_entry.1, "Jump to screen");
        // The jump key legend must include all screen jump keys.
        let legend = &jump_entry.0;
        assert!(
            legend.contains("1-9"),
            "jump key legend should contain '1-9', got: {legend}"
        );
        // With 14 screens, we expect shifted symbols for screens 11-14.
        let screen_count = crate::tui_screens::ALL_SCREEN_IDS.len();
        if screen_count > 10 {
            assert!(
                legend.contains('!'),
                "with {screen_count} screens, legend should contain '!' for screen 11, got: {legend}"
            );
        }
    }

    #[test]
    fn tab_count_matches_screens() {
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), ALL_SCREEN_IDS.len());
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), 14);
    }

    #[test]
    fn render_tab_bar_with_effects_enabled() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 1, &mut pool);
        render_tab_bar(
            MailScreenId::Dashboard,
            true,
            &mut frame,
            Rect::new(0, 0, 120, 1),
        );
    }

    #[test]
    fn render_tab_bar_with_effects_disabled() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 1, &mut pool);
        render_tab_bar(
            MailScreenId::Dashboard,
            false,
            &mut frame,
            Rect::new(0, 0, 120, 1),
        );
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

    #[test]
    fn tab_hit_slots_cover_all_visible_tabs_normal_width() {
        let dispatcher = crate::tui_hit_regions::MouseDispatcher::new();
        let area = Rect::new(0, 0, 200, 1); // Wide enough for all tabs
        record_tab_hit_slots(area, &dispatcher);
        let mut found = 0;
        for i in 0..ALL_SCREEN_IDS.len() {
            if dispatcher.tab_slot(i).is_some() {
                found += 1;
            }
        }
        assert_eq!(found, ALL_SCREEN_IDS.len(), "all tabs should have hit slots at width 200");
    }

    #[test]
    fn tab_hit_slots_ultra_compact_fits_more_tabs() {
        // At 40 cols, compact mode shows short labels; at 30 cols, ultra-compact
        // shows key-only. Ultra-compact should fit more tabs.
        let d_compact = crate::tui_hit_regions::MouseDispatcher::new();
        record_tab_hit_slots(Rect::new(0, 0, 50, 1), &d_compact);

        let d_ultra = crate::tui_hit_regions::MouseDispatcher::new();
        record_tab_hit_slots(Rect::new(0, 0, 30, 1), &d_ultra);

        let count = |d: &crate::tui_hit_regions::MouseDispatcher| -> usize {
            (0..ALL_SCREEN_IDS.len())
                .filter(|&i| d.tab_slot(i).is_some())
                .count()
        };

        // Ultra-compact at 30 should fit at least as many as compact at 50.
        assert!(
            count(&d_ultra) >= count(&d_compact),
            "ultra-compact should fit more or equal tabs"
        );
    }

    #[test]
    fn tab_hit_slots_no_overlap() {
        let dispatcher = crate::tui_hit_regions::MouseDispatcher::new();
        record_tab_hit_slots(Rect::new(0, 0, 200, 1), &dispatcher);

        let mut prev_end: u16 = 0;
        for i in 0..ALL_SCREEN_IDS.len() {
            if let Some((x_start, x_end, _y)) = dispatcher.tab_slot(i) {
                assert!(
                    x_start >= prev_end,
                    "tab {i} overlaps previous: starts at {x_start}, prev ended at {prev_end}"
                );
                assert!(x_end > x_start, "tab {i} has zero width");
                prev_end = x_end;
            }
        }
    }
}
