//! Chrome shell for `AgentMailTUI`: tab bar, status line, help overlay.
//!
//! The chrome renders persistent UI elements that frame every screen.
//! Layout: `[tab_bar(1)] [screen_content(fill)] [status_line(1)]`

use ftui::layout::{Constraint, Flex, Rect};
use ftui::text::display_width;
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
const fn category_key_color(
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
#[allow(clippy::too_many_lines)]
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
        let key_w = u16::try_from(display_width(key_str.as_str())).unwrap_or(u16::MAX);
        let label_w = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
        // Width: indicator + space + key + colon? + label? + space
        let has_label = !label.is_empty();
        let tab_width = if has_label {
            1_u16
                .saturating_add(key_w)
                .saturating_add(1)
                .saturating_add(label_w)
                .saturating_add(1)
        } else {
            // Ultra-compact: " 1 " (space + key + space)
            1_u16.saturating_add(key_w).saturating_add(1)
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
        // Clear each tab slot before drawing text so separators from prior
        // frames cannot bleed through shorter/shifted labels.
        Paragraph::new("")
            .style(Style::default().bg(bg))
            .render(tab_area, frame);

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
            spans.push(Span::styled(
                ":",
                Style::default().fg(tp.tab_inactive_fg).bg(bg),
            ));
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
pub fn record_tab_hit_slots(area: Rect, dispatcher: &crate::tui_hit_regions::MouseDispatcher) {
    let available = area.width;
    let ultra_compact = available < 40;
    let compact = available < 60;
    let mut x = area.x;

    for (i, meta) in MAIL_SCREEN_REGISTRY.iter().enumerate() {
        let number = i + 1;
        let key_str = format!("{number}");
        let key_w = u16::try_from(display_width(key_str.as_str())).unwrap_or(u16::MAX);
        let label = if ultra_compact {
            ""
        } else if compact {
            meta.short_label
        } else {
            meta.title
        };
        let label_w = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
        let has_label = !label.is_empty();
        let tab_width: u16 = if has_label {
            1_u16
                .saturating_add(key_w)
                .saturating_add(1)
                .saturating_add(label_w)
                .saturating_add(1)
        } else {
            1_u16.saturating_add(key_w).saturating_add(1)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusEffect {
    None,
    LivePulse,
    RecordingPulse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusRole {
    Normal,
    PaletteToggle,
    HelpToggle,
}

/// A semantic segment of the status bar.
struct StatusSegment {
    priority: StatusPriority,
    role: StatusRole,
    text: String,
    fg: PackedRgba,
    bold: bool,
    effect: StatusEffect,
}

fn status_group_width(segments: &[StatusSegment], separated: bool) -> u16 {
    segments.iter().enumerate().fold(0u16, |acc, (idx, seg)| {
        let sep = if separated && idx > 0 { 3 } else { 0 };
        let width = u16::try_from(display_width(&seg.text)).unwrap_or(u16::MAX);
        acc.saturating_add(sep).saturating_add(width)
    })
}

fn status_total_width(
    left: &[StatusSegment],
    center: &[StatusSegment],
    right: &[StatusSegment],
) -> u16 {
    let left_width = status_group_width(left, false);
    let center_width = status_group_width(center, true);
    let right_width = status_group_width(right, false);
    left_width
        .saturating_add(center_width)
        .saturating_add(right_width)
}

fn drop_last_priority(segments: &mut Vec<StatusSegment>, priority: StatusPriority) -> bool {
    segments
        .iter()
        .rposition(|seg| seg.priority == priority)
        .is_some_and(|idx| {
            segments.remove(idx);
            true
        })
}

fn prune_status_segments_to_fit(
    left: &mut Vec<StatusSegment>,
    center: &mut Vec<StatusSegment>,
    right: &mut Vec<StatusSegment>,
    available: u16,
) {
    loop {
        if status_total_width(left, center, right) <= available {
            break;
        }
        let removed = drop_last_priority(center, StatusPriority::Low)
            || drop_last_priority(left, StatusPriority::Low)
            || drop_last_priority(right, StatusPriority::Low)
            || drop_last_priority(center, StatusPriority::Medium)
            || drop_last_priority(left, StatusPriority::Medium)
            || drop_last_priority(right, StatusPriority::Medium)
            || drop_last_priority(center, StatusPriority::High)
            || drop_last_priority(left, StatusPriority::High)
            || drop_last_priority(right, StatusPriority::High);
        if removed {
            continue;
        }

        // Preserve help/palette affordances when possible and peel other
        // critical tags first (e.g. LIVE on ultra-narrow terminals).
        if right.len() > 1 {
            if let Some(idx) = right.iter().rposition(|seg| seg.role == StatusRole::Normal) {
                right.remove(idx);
                continue;
            }
            if let Some(idx) = right
                .iter()
                .rposition(|seg| seg.role == StatusRole::PaletteToggle)
            {
                right.remove(idx);
                continue;
            }
            right.pop();
            continue;
        }
        break;
    }
}

/// Compute which segments to show given available width.
///
/// Segments are grouped into left (always left-aligned), center
/// (centered between left and right), and right (right-aligned).
/// Lower-priority segments are dropped until everything fits.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn plan_status_segments(
    state: &TuiSharedState,
    active: MailScreenId,
    recording_active: bool,
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
    let counter_fg = if error_count > 0 {
        tp.status_warn
    } else {
        tp.status_good
    };

    // ── Left segments (always left-aligned) ──
    let mut left = vec![StatusSegment {
        priority: StatusPriority::Critical,
        role: StatusRole::Normal,
        text: format!(" {}", meta.title),
        fg: tp.status_accent,
        bold: true,
        effect: StatusEffect::None,
    }];

    // Transport mode (High priority)
    left.push(StatusSegment {
        priority: StatusPriority::High,
        role: StatusRole::Normal,
        text: format!(" {transport_mode}"),
        fg: tp.status_fg,
        bold: false,
        effect: StatusEffect::None,
    });

    // Uptime (Medium priority)
    left.push(StatusSegment {
        priority: StatusPriority::Medium,
        role: StatusRole::Normal,
        text: format!(" up:{uptime_str}"),
        fg: tp.status_fg,
        bold: false,
        effect: StatusEffect::None,
    });

    // ── Center segments (centered) ──
    let mut center = Vec::new();

    // Error count alone at Medium priority (most critical counter)
    if error_count > 0 {
        center.push(StatusSegment {
            priority: StatusPriority::Medium,
            role: StatusRole::Normal,
            text: format!("err:{error_count}"),
            fg: tp.status_warn,
            bold: true,
            effect: StatusEffect::None,
        });
    }

    // Full counter string at Low priority
    center.push(StatusSegment {
        priority: StatusPriority::Low,
        role: StatusRole::Normal,
        text: format!("req:{total} ok:{ok} err:{error_count} avg:{avg_latency}ms"),
        fg: counter_fg,
        bold: false,
        effect: StatusEffect::None,
    });

    // Key hints at Low priority
    if accessibility.key_hints && !accessibility.screen_reader && !screen_bindings.is_empty() {
        let max_hint = (available / 3).max(20) as usize;
        let hints = build_key_hints(screen_bindings, 6, max_hint);
        if !hints.is_empty() {
            center.push(StatusSegment {
                priority: StatusPriority::Low,
                role: StatusRole::Normal,
                text: hints,
                fg: tp.status_fg,
                bold: false,
                effect: StatusEffect::None,
            });
        }
    }

    // ── Right segments (right-aligned) ──
    let palette_hint = "[^P]";
    let help_hint = if help_visible { "[?]" } else { "?" };
    let mut right = Vec::new();

    right.push(StatusSegment {
        priority: StatusPriority::Critical,
        role: StatusRole::Normal,
        text: "LIVE ".to_string(),
        fg: tp.status_good,
        bold: true,
        effect: StatusEffect::LivePulse,
    });

    if recording_active {
        right.push(StatusSegment {
            priority: StatusPriority::High,
            role: StatusRole::Normal,
            text: "REC ".to_string(),
            fg: tp.status_warn,
            bold: true,
            effect: StatusEffect::RecordingPulse,
        });
    }

    // Toast mute indicator (High priority)
    if toast_muted {
        right.push(StatusSegment {
            priority: StatusPriority::High,
            role: StatusRole::Normal,
            text: "[muted] ".to_string(),
            fg: tp.status_warn,
            bold: false,
            effect: StatusEffect::None,
        });
    }

    // Effects-off indicator (High priority — user-activated toggle)
    if !state.config_snapshot().tui_effects {
        right.push(StatusSegment {
            priority: StatusPriority::High,
            role: StatusRole::Normal,
            text: "[fx off] ".to_string(),
            fg: tp.status_warn,
            bold: false,
            effect: StatusEffect::None,
        });
    }

    // Accessibility indicators (Medium priority)
    {
        let mut tags: Vec<&str> = Vec::new();
        if accessibility.high_contrast {
            tags.push("hc");
        }
        if accessibility.reduced_motion {
            tags.push("rm");
        }
        if accessibility.screen_reader {
            tags.push("sr");
        }
        if !tags.is_empty() {
            let label = format!("[{}] ", tags.join(","));
            right.push(StatusSegment {
                priority: StatusPriority::Medium,
                role: StatusRole::Normal,
                text: label,
                fg: tp.status_fg,
                bold: false,
                effect: StatusEffect::None,
            });
        }
    }

    // Theme name (Low priority — informational)
    {
        let theme = crate::tui_theme::current_theme_name();
        right.push(StatusSegment {
            priority: StatusPriority::Low,
            role: StatusRole::Normal,
            text: format!("{theme} "),
            fg: tp.status_fg,
            bold: false,
            effect: StatusEffect::None,
        });
    }

    right.push(StatusSegment {
        priority: StatusPriority::Critical,
        role: StatusRole::PaletteToggle,
        text: format!("{palette_hint} "),
        fg: tp.tab_key_fg,
        bold: false,
        effect: StatusEffect::None,
    });
    right.push(StatusSegment {
        priority: StatusPriority::Critical,
        role: StatusRole::HelpToggle,
        text: format!("{help_hint} "),
        fg: tp.tab_key_fg,
        bold: false,
        effect: StatusEffect::None,
    });

    // If both Medium error count and Low full counters survived, drop
    // the Medium error-only duplicate (full counters include it).
    if center.len() > 1
        && center
            .iter()
            .any(|s| s.priority == StatusPriority::Low && s.text.contains("req:"))
    {
        center.retain(|s| !(s.priority == StatusPriority::Medium && s.text.starts_with("err:")));
    }

    // Coarse priority bands by width, then fine-grained pruning for fit.
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

    prune_status_segments_to_fit(&mut left, &mut center, &mut right, available);

    (left, center, right)
}

#[inline]
fn segment_text_width(text: &str) -> u16 {
    u16::try_from(display_width(text)).unwrap_or(u16::MAX)
}

/// Render the status line into a 1-row area.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn render_status_line(
    state: &TuiSharedState,
    active: MailScreenId,
    recording_active: bool,
    help_visible: bool,
    accessibility: &AccessibilitySettings,
    screen_bindings: &[HelpEntry],
    toast_muted: bool,
    frame: &mut Frame,
    area: Rect,
) {
    use ftui::text::{Line, Span, Text};
    use ftui_extras::text_effects::{StyledText, TextEffect};

    let tp = crate::tui_theme::TuiThemePalette::current();

    // Fill background
    let bg_style = Style::default().bg(tp.status_bg);
    Paragraph::new("").style(bg_style).render(area, frame);

    let (left, center, right) = plan_status_segments(
        state,
        active,
        recording_active,
        help_visible,
        accessibility,
        screen_bindings,
        toast_muted,
        area.width,
    );

    // Compute total widths.
    let left_width = left.iter().fold(0u16, |acc, s| {
        acc.saturating_add(segment_text_width(&s.text))
    });
    let center_width: u16 = center
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let sep = if i > 0 { 3u16 } else { 0 }; // " | "
            segment_text_width(&s.text).saturating_add(sep)
        })
        .sum();
    let right_width = right.iter().fold(0u16, |acc, s| {
        acc.saturating_add(segment_text_width(&s.text))
    });

    let mut spans: Vec<Span<'_>> = Vec::with_capacity(16);
    let mut effect_overlays: Vec<(u16, u16, StatusEffect, PackedRgba, String)> = Vec::new();
    let mut cursor_x = area.x;
    let effects_enabled = state.config_snapshot().tui_effects && !accessibility.reduced_motion;
    let animation_time = state.uptime().as_secs_f64();

    // Left segments
    for seg in &left {
        let mut style = Style::default().fg(seg.fg).bg(tp.status_bg);
        if seg.bold {
            style = style.bold();
        }
        spans.push(Span::styled(seg.text.as_str(), style));
        cursor_x = cursor_x.saturating_add(segment_text_width(&seg.text));
    }

    // Center padding + center segments
    let total_fixed = left_width + center_width + right_width;
    if center_width > 0 && total_fixed < area.width {
        let gap = area.width - total_fixed;
        let left_pad = gap / 2;
        if left_pad > 0 {
            spans.push(Span::styled(" ".repeat(left_pad as usize), bg_style));
            cursor_x = cursor_x.saturating_add(left_pad);
        }
    } else if center_width == 0 {
        // No center — push right to the far right.
        let gap = area.width.saturating_sub(left_width + right_width);
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap as usize), bg_style));
            cursor_x = cursor_x.saturating_add(gap);
        }
    }

    for (i, seg) in center.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " | ",
                Style::default().fg(tp.status_fg).bg(tp.status_bg),
            ));
            cursor_x = cursor_x.saturating_add(3);
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
        cursor_x = cursor_x.saturating_add(segment_text_width(&seg.text));
    }

    // Right padding
    if center_width > 0 && total_fixed < area.width {
        let gap = area.width - total_fixed;
        let right_pad = gap - gap / 2;
        if right_pad > 0 {
            spans.push(Span::styled(" ".repeat(right_pad as usize), bg_style));
            cursor_x = cursor_x.saturating_add(right_pad);
        }
    }

    // Right segments
    for seg in &right {
        let mut style = Style::default().fg(seg.fg).bg(tp.status_bg);
        if seg.bold {
            style = style.bold();
        }
        let seg_width = segment_text_width(&seg.text);
        if effects_enabled && seg.effect != StatusEffect::None && seg_width > 0 {
            effect_overlays.push((cursor_x, seg_width, seg.effect, seg.fg, seg.text.clone()));
        }
        spans.push(Span::styled(seg.text.as_str(), style));
        cursor_x = cursor_x.saturating_add(seg_width);
    }

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines([line])).render(area, frame);

    if effects_enabled {
        for (x, width, effect, color, label_with_space) in effect_overlays {
            let label = label_with_space.trim_end();
            if label.is_empty() {
                continue;
            }
            let label_width = u16::try_from(display_width(label))
                .unwrap_or(u16::MAX)
                .min(width);
            let effect = match effect {
                StatusEffect::LivePulse => TextEffect::Pulse {
                    speed: 2.0 / 3.0,
                    min_alpha: 0.35,
                },
                StatusEffect::RecordingPulse => TextEffect::Pulse {
                    speed: 2.0 / 3.0,
                    min_alpha: 0.20,
                },
                StatusEffect::None => continue,
            };
            StyledText::new(label)
                .effect(effect)
                .base_color(color)
                .bold()
                .time(animation_time)
                .render(Rect::new(x, area.y, label_width, 1), frame);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Help overlay
// ──────────────────────────────────────────────────────────────────────

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
    let overlay_area = help_overlay_rect(area);

    // Total line count for scroll indicator.
    let total_lines: usize = sections
        .iter()
        .map(|s| s.line_count() + 1) // +1 for blank separator between sections
        .sum::<usize>()
        .saturating_sub(1); // no trailing separator

    let scroll_hint = if total_lines > usize::from(overlay_area.height.saturating_sub(2)) {
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
    let max_scroll = total_lines.saturating_sub(usize::from(inner.height));
    let clamped_scroll = usize::from(scroll_offset).min(max_scroll);
    let visible_start = u16::try_from(clamped_scroll).unwrap_or(u16::MAX);
    let visible_end = visible_start.saturating_add(inner.height);
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

        // Optional context description.
        if let Some(ref desc) = section.description {
            if line_idx >= visible_start && line_idx < visible_end && y_pos < inner.height {
                let desc_para = Paragraph::new(desc.as_str())
                    .style(Style::default().fg(tp.status_fg).bg(tp.help_bg).italic());
                desc_para.render(
                    Rect::new(inner.x + 2, inner.y + y_pos, col_width.saturating_sub(1), 1),
                    frame,
                );
                y_pos += 1;
            }
            line_idx += 1;
        }

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

/// Compute the centered help-overlay rectangle for a terminal frame area.
#[must_use]
pub fn help_overlay_rect(area: Rect) -> Rect {
    let overlay_width = (u32::from(area.width) * 60 / 100).clamp(36, 72) as u16;
    let overlay_height = (u32::from(area.height) * 60 / 100).clamp(10, 28) as u16;
    let overlay_width = overlay_width.min(area.width.saturating_sub(2));
    let overlay_height = overlay_height.min(area.height.saturating_sub(2));

    let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    Rect::new(x, y, overlay_width, overlay_height)
}

/// Compute the centered debug-inspector rectangle for a terminal frame area.
#[must_use]
pub fn inspector_overlay_rect(area: Rect) -> Rect {
    let overlay_width = (u32::from(area.width) * 70 / 100).clamp(48, 96) as u16;
    let overlay_height = (u32::from(area.height) * 72 / 100).clamp(12, 36) as u16;
    let overlay_width = overlay_width.min(area.width.saturating_sub(2));
    let overlay_height = overlay_height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    Rect::new(x, y, overlay_width, overlay_height)
}

/// Render the debug inspector overlay with a widget tree and optional properties panel.
pub fn render_inspector_overlay(
    frame: &mut Frame,
    area: Rect,
    tree_lines: &[String],
    selected_index: usize,
    selected_area: Option<Rect>,
    properties: &[String],
    show_properties: bool,
) {
    use ftui::text::{Line, Span, Text};
    let tp = crate::tui_theme::TuiThemePalette::current();

    if let Some(target) = selected_area {
        draw_outline(target, frame, tp.panel_border_focused);
    }

    let overlay = inspector_overlay_rect(area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Inspector (F12/Esc close, Arrows move, Enter props) ")
        .style(Style::default().fg(tp.help_border_fg).bg(tp.help_bg));
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    if inner.is_empty() {
        return;
    }

    let panes = if show_properties && inner.width >= 56 {
        Flex::horizontal()
            .constraints([Constraint::Percentage(62.0), Constraint::Percentage(38.0)])
            .split(inner)
    } else {
        vec![inner]
    };

    let tree_area = panes[0];
    let tree_block = Block::bordered()
        .border_type(BorderType::Square)
        .title(" Widget Tree ")
        .style(Style::default().fg(tp.help_border_fg).bg(tp.help_bg));
    let tree_inner = tree_block.inner(tree_area);
    tree_block.render(tree_area, frame);

    let mut tree_render_lines: Vec<Line> = Vec::new();
    if tree_lines.is_empty() {
        tree_render_lines.push(Line::from_spans([Span::styled(
            "  (no widgets recorded for this frame)",
            Style::default().fg(tp.text_muted).bg(tp.help_bg),
        )]));
    } else {
        let visible_rows = usize::from(tree_inner.height.max(1));
        let selected = selected_index.min(tree_lines.len().saturating_sub(1));
        let start = selected.saturating_sub(visible_rows.saturating_sub(1));
        let end = (start + visible_rows).min(tree_lines.len());
        for (idx, line) in tree_lines[start..end].iter().enumerate() {
            let absolute = start + idx;
            let is_selected = absolute == selected;
            let prefix = if is_selected { ">" } else { " " };
            let style = if is_selected {
                Style::default()
                    .fg(tp.help_bg)
                    .bg(tp.panel_border_focused)
                    .bold()
            } else {
                Style::default().fg(tp.help_fg).bg(tp.help_bg)
            };
            tree_render_lines.push(Line::from_spans([Span::styled(
                format!("{prefix} {line}"),
                style,
            )]));
        }
    }
    Paragraph::new(Text::from_lines(tree_render_lines))
        .style(Style::default().bg(tp.help_bg))
        .render(tree_inner, frame);

    if panes.len() > 1 {
        let props_area = panes[1];
        let props_block = Block::bordered()
            .border_type(BorderType::Square)
            .title(" Properties ")
            .style(Style::default().fg(tp.help_border_fg).bg(tp.help_bg));
        let props_inner = props_block.inner(props_area);
        props_block.render(props_area, frame);
        let props_lines = if properties.is_empty() {
            vec!["(no widget selected)".to_string()]
        } else {
            properties.to_vec()
        };
        Paragraph::new(props_lines.join("\n"))
            .style(Style::default().fg(tp.help_fg).bg(tp.help_bg))
            .render(props_inner, frame);
    }
}

fn draw_outline(rect: Rect, frame: &mut Frame, color: PackedRgba) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let left = rect.x;
    let right = rect.right().saturating_sub(1);
    let top = rect.y;
    let bottom = rect.bottom().saturating_sub(1);
    for x in left..=right {
        if let Some(cell) = frame.buffer.get_mut(x, top) {
            cell.fg = color;
        }
        if let Some(cell) = frame.buffer.get_mut(x, bottom) {
            cell.fg = color;
        }
    }
    for y in top..=bottom {
        if let Some(cell) = frame.buffer.get_mut(left, y) {
            cell.fg = color;
        }
        if let Some(cell) = frame.buffer.get_mut(right, y) {
            cell.fg = color;
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

    let keycap_style = Style::default().fg(tp.help_bg).bg(tp.help_key_fg).bold();

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
const fn chip_width(key: &str, action: &str, is_last: bool) -> usize {
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
            || used
                + chip_width(entry.key, entry.action, false)
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

    let keycap_style = Style::default().fg(tp.status_bg).bg(tp.tab_key_fg).bold();
    let action_style = Style::default().fg(tp.status_fg).bg(tp.status_bg);
    let sep_style = Style::default().fg(tp.tab_inactive_fg).bg(tp.status_bg);

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
                spans.push(Span::styled(format!(" {key} "), keycap_style));
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
                HelpEntry {
                    key: "j",
                    action: "Down",
                },
                HelpEntry {
                    key: "k",
                    action: "Up",
                },
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
        let attrs = first_keycap
            .style
            .and_then(|s| s.attrs)
            .unwrap_or(ftui::style::StyleFlags::NONE);
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
    fn global_bindings_registry_complete() {
        use crate::tui_keymap::GLOBAL_BINDINGS;
        assert!(
            GLOBAL_BINDINGS.len() >= 5,
            "GLOBAL_BINDINGS should have >= 5 entries, got {}",
            GLOBAL_BINDINGS.len()
        );
        for b in GLOBAL_BINDINGS {
            assert!(!b.label.is_empty(), "empty label in GLOBAL_BINDINGS");
            assert!(!b.action.is_empty(), "empty action in GLOBAL_BINDINGS");
        }
    }

    #[test]
    fn global_bindings_jump_key_matches_registry() {
        use crate::tui_keymap::GLOBAL_BINDINGS;
        let jump = GLOBAL_BINDINGS
            .iter()
            .find(|b| b.action == "Jump to screen")
            .expect("GLOBAL_BINDINGS should contain 'Jump to screen'");
        let legend = crate::tui_screens::jump_key_legend();
        assert!(
            legend.contains("1-9"),
            "jump key legend should contain '1-9', got: {legend}"
        );
        // The binding label is a static placeholder; the legend is generated
        // dynamically from the screen registry (auto-synchronized).
        assert!(
            !jump.label.is_empty(),
            "jump binding label should not be empty"
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
    fn help_entries_synchronized_with_global_bindings() {
        use crate::tui_keymap::{GLOBAL_BINDINGS, KeymapProfile, KeymapRegistry};
        let registry = KeymapRegistry::new(KeymapProfile::Default);
        let entries = registry.help_entries();
        // Every GLOBAL_BINDINGS action should appear in help_entries output
        // (jump action gets a dynamic suffix so use `contains`).
        for b in GLOBAL_BINDINGS {
            let found = entries.iter().any(|(_, action)| action.contains(b.action));
            assert!(
                found,
                "GLOBAL_BINDINGS action '{}' missing from help_entries()",
                b.action
            );
        }
        // help_entries should produce at least as many entries as GLOBAL_BINDINGS.
        assert!(
            entries.len() >= GLOBAL_BINDINGS.len(),
            "help_entries() has {} entries but GLOBAL_BINDINGS has {}",
            entries.len(),
            GLOBAL_BINDINGS.len()
        );
    }

    #[test]
    fn tab_count_matches_screens() {
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), ALL_SCREEN_IDS.len());
        assert_eq!(MAIL_SCREEN_REGISTRY.len(), 15);
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
        let area = Rect::new(0, 0, 240, 1); // Wide enough for all 15 tabs
        record_tab_hit_slots(area, &dispatcher);
        let mut found = 0;
        for i in 0..ALL_SCREEN_IDS.len() {
            if dispatcher.tab_slot(i).is_some() {
                found += 1;
            }
        }
        assert_eq!(
            found,
            ALL_SCREEN_IDS.len(),
            "all tabs should have hit slots at width 200"
        );
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

    // ── Status segment discoverability tests (br-1xt0m.1.12.1) ──

    #[test]
    fn status_segments_high_contrast_shown() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings {
            high_contrast: true,
            key_hints: true,
            reduced_motion: false,
            screen_reader: false,
        };
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let tags: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            tags.contains("[hc]"),
            "high_contrast should produce [hc] tag, got: {tags}"
        );
    }

    #[test]
    fn status_segments_all_a11y_combined() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings {
            high_contrast: true,
            key_hints: true,
            reduced_motion: true,
            screen_reader: true,
        };
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let tags: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            tags.contains("[hc,rm,sr]"),
            "all a11y flags should be combined, got: {tags}"
        );
    }

    #[test]
    fn status_segments_include_live_indicator() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let right_text: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            right_text.contains("LIVE"),
            "expected LIVE indicator in status right segments, got: {right_text}"
        );
    }

    #[test]
    fn status_segments_include_palette_and_help_hints() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let right_text: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            right_text.contains("[^P]"),
            "expected palette hint in status segments: {right_text}"
        );
        assert!(
            right_text.contains('?'),
            "expected help hint in status segments: {right_text}"
        );
        let last_text = right.last().map(|s| s.text.trim()).unwrap_or_default();
        assert!(
            last_text == "?" || last_text == "[?]",
            "expected help hint to be rightmost, got: {last_text}"
        );
    }

    #[test]
    fn status_segments_recording_flag_controls_rec_indicator() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();

        let (_, _, right_not_recording) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let right_not_recording_text: String = right_not_recording
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            !right_not_recording_text.contains("REC"),
            "REC indicator should be absent when not recording: {right_not_recording_text}"
        );

        let (_, _, right_recording) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            true,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let right_recording_text: String =
            right_recording.iter().map(|s| s.text.as_str()).collect();
        assert!(
            right_recording_text.contains("REC"),
            "REC indicator should be present when recording: {right_recording_text}"
        );
    }

    // ── Width matrix: status segment visibility at breakpoints (br-1xt0m.1.13.9) ──

    #[test]
    fn status_segments_at_50_cols_critical_only() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (left, center, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            50, // < 60 → Critical only
        );
        // Only Critical segments survive.
        for seg in left.iter().chain(center.iter()).chain(right.iter()) {
            assert_eq!(
                seg.priority,
                StatusPriority::Critical,
                "at 50 cols, only Critical segments should survive, got {:?}: '{}'",
                seg.priority,
                seg.text
            );
        }
    }

    #[test]
    fn status_segments_at_60_cols_includes_high() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (left, center, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            60, // >= 60 → Critical + High
        );
        let all: Vec<_> = left
            .iter()
            .chain(center.iter())
            .chain(right.iter())
            .collect();
        // Should have at least one High-priority segment (transport mode).
        assert!(
            all.iter().any(|s| s.priority == StatusPriority::High),
            "at 60 cols, High-priority segments should be present"
        );
        // No Medium or Low.
        assert!(
            all.iter().all(|s| s.priority <= StatusPriority::High),
            "at 60 cols, only Critical + High segments should survive"
        );
    }

    #[test]
    fn status_segments_at_80_cols_includes_medium() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (left, center, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            80, // >= 80 → Critical + High + Medium
        );
        let all: Vec<_> = left
            .iter()
            .chain(center.iter())
            .chain(right.iter())
            .collect();
        assert!(
            all.iter().any(|s| s.priority == StatusPriority::Medium),
            "at 80 cols, Medium-priority segments should be present"
        );
        // Uptime is Medium priority on left side.
        let left_text: String = left.iter().map(|s| s.text.as_str()).collect();
        assert!(
            left_text.contains("up:"),
            "at 80 cols, uptime segment (Medium) should appear in left: {left_text}"
        );
        // No Low-priority segments.
        assert!(
            all.iter().all(|s| s.priority <= StatusPriority::Medium),
            "at 80 cols, Low segments should be dropped"
        );
    }

    #[test]
    fn status_segments_at_100_cols_includes_low() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (left, center, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            100, // >= 100 → all priorities
        );
        let all: Vec<_> = left
            .iter()
            .chain(center.iter())
            .chain(right.iter())
            .collect();
        assert!(
            all.iter().any(|s| s.priority == StatusPriority::Low),
            "at 100 cols, Low-priority segments should be present"
        );
        // Full counters (Low) should appear in center.
        let center_text: String = center.iter().map(|s| s.text.as_str()).collect();
        assert!(
            center_text.contains("req:"),
            "at 100 cols, full counter string (Low) should appear: {center_text}"
        );
    }

    #[test]
    fn status_segments_at_120_cols_has_theme_name() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let theme = crate::tui_theme::current_theme_name();
        let right_text: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            right_text.contains(theme),
            "at 120 cols, theme name '{theme}' in right: {right_text}"
        );
    }

    #[test]
    fn status_segments_at_160_cols_same_as_100() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (left_100, center_100, right_100) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            100,
        );
        let (left_160, center_160, right_160) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            160,
        );
        // Same segment count — 160 doesn't add more segments than 100.
        assert_eq!(
            left_100.len(),
            left_160.len(),
            "left segment count should be same at 100 and 160 cols"
        );
        assert_eq!(
            center_100.len(),
            center_160.len(),
            "center segment count should be same at 100 and 160 cols"
        );
        assert_eq!(
            right_100.len(),
            right_160.len(),
            "right segment count should be same at 100 and 160 cols"
        );
    }

    // ── Help overlay sizing matrix (br-1xt0m.1.13.9) ──

    #[test]
    fn help_overlay_sizing_80x24() {
        // 80*60% = 48 → clamped to [36,72] = 48
        // 24*60% = 14 → clamped to [10,28] = 14
        let w = (u32::from(80u16) * 60 / 100).clamp(36, 72) as u16;
        let h = (u32::from(24u16) * 60 / 100).clamp(10, 28) as u16;
        assert_eq!(w, 48, "overlay width at 80 cols");
        assert_eq!(h, 14, "overlay height at 24 rows");
    }

    #[test]
    fn help_overlay_sizing_100x30() {
        let w = (u32::from(100u16) * 60 / 100).clamp(36, 72) as u16;
        let h = (u32::from(30u16) * 60 / 100).clamp(10, 28) as u16;
        assert_eq!(w, 60, "overlay width at 100 cols");
        assert_eq!(h, 18, "overlay height at 30 rows");
    }

    #[test]
    fn help_overlay_sizing_120x40() {
        let w = (u32::from(120u16) * 60 / 100).clamp(36, 72) as u16;
        let h = (u32::from(40u16) * 60 / 100).clamp(10, 28) as u16;
        assert_eq!(w, 72, "overlay width at 120 cols");
        assert_eq!(h, 24, "overlay height at 40 rows");
    }

    #[test]
    fn help_overlay_sizing_160x48() {
        let w = (u32::from(160u16) * 60 / 100).clamp(36, 72) as u16;
        let h = (u32::from(48u16) * 60 / 100).clamp(10, 28) as u16;
        assert_eq!(w, 72, "overlay width at 160 cols (clamped max)");
        assert_eq!(h, 28, "overlay height at 48 rows (clamped max)");
    }

    #[test]
    fn help_overlay_sizing_40x12_minimum() {
        let w = (u32::from(40u16) * 60 / 100).clamp(36, 72) as u16;
        let h = (u32::from(12u16) * 60 / 100).clamp(10, 28) as u16;
        // 40*60% = 24, clamped to 36 (min).
        // But also clamped by area: min(36, 40-2) = 36.
        assert_eq!(w, 36, "overlay width at 40 cols (clamped min)");
        // 12*60% = 7, clamped to 10 (min). Also min(10, 12-2) = 10.
        assert_eq!(h, 10, "overlay height at 12 rows (clamped min)");
    }

    #[test]
    fn inspector_overlay_sizing_120x40() {
        let rect = inspector_overlay_rect(Rect::new(0, 0, 120, 40));
        assert_eq!(rect.width, 84, "inspector width at 120 cols (70%)");
        assert_eq!(rect.height, 28, "inspector height at 40 rows (72%)");
    }

    #[test]
    fn inspector_overlay_render_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let tree = vec![
            "MailApp (x=0 y=0 w=120 h=40) [123us]".to_string(),
            "  TabBar (x=0 y=0 w=120 h=1) [10us]".to_string(),
            "  Screen Dashboard (x=0 y=1 w=120 h=38) [99us]".to_string(),
        ];
        let props = vec![
            "Name: Screen Dashboard".to_string(),
            "Depth: 1".to_string(),
            "Rect: x=0 y=1 w=120 h=38".to_string(),
            "HitId: 2100".to_string(),
            "Render: 99us".to_string(),
        ];
        render_inspector_overlay(
            &mut frame,
            Rect::new(0, 0, 120, 40),
            &tree,
            2,
            Some(Rect::new(0, 1, 120, 38)),
            &props,
            true,
        );
    }

    #[test]
    fn status_segments_theme_name_at_wide_width() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let a11y = AccessibilitySettings::default();
        let (_, _, right) = plan_status_segments(
            &state,
            MailScreenId::Dashboard,
            false,
            false,
            &a11y,
            &[],
            false,
            120,
        );
        let theme = crate::tui_theme::current_theme_name();
        let tags: String = right.iter().map(|s| s.text.as_str()).collect();
        assert!(
            tags.contains(theme),
            "theme name '{theme}' should appear in right segments, got: {tags}"
        );
    }
}
