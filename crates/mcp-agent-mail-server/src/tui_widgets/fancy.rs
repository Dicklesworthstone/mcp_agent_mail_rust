//! Fancy widgets for the Frankenstein theme (bolts, stitches, peeking eyes,
//! status badges, summary footers).

use ftui::layout::Rect;
use ftui::style::Style;
use ftui::text::{Line, Span};
use ftui::widgets::Widget;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Cell, PackedRgba};

// ═══════════════════════════════════════════════════════════════════════════════
// StatusBadge — compact icon+label inline widget
// ═══════════════════════════════════════════════════════════════════════════════

/// Compact 1-line widget rendering `● ACTIVE` style badges.
///
/// Suitable for embedding in table cells or summary headers.
#[derive(Debug, Clone)]
pub struct StatusBadge<'a> {
    icon: &'a str,
    label: &'a str,
    style: Style,
}

impl<'a> StatusBadge<'a> {
    #[must_use]
    pub const fn new(icon: &'a str, label: &'a str) -> Self {
        Self {
            icon,
            label,
            style: Style::new(),
        }
    }

    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Format as a plain string (for table cell content).
    #[must_use]
    pub fn to_string_repr(&self) -> String {
        format!("{} {}", self.icon, self.label)
    }
}

impl Widget for StatusBadge<'_> {
    fn render(&self, area: Rect, frame: &mut ftui::Frame) {
        if area.width < 3 || area.height == 0 {
            return;
        }
        let text = format!("{} {}", self.icon, self.label);
        let truncated = super::truncate_width(&text, area.width);
        let line = Line::styled(truncated, self.style);
        Paragraph::new(line).render(area, frame);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SummaryFooter — 1-line stats bar with colored separators
// ═══════════════════════════════════════════════════════════════════════════════

/// A 1-line footer bar rendering key=value stats separated by ` │ `.
///
/// Example output: `6 projects │ 18 agents │ 892 msgs │ 5 reserv`
#[derive(Debug, Clone)]
pub struct SummaryFooter<'a> {
    items: &'a [(&'a str, &'a str, PackedRgba)],
    separator_color: PackedRgba,
}

impl<'a> SummaryFooter<'a> {
    #[must_use]
    pub const fn new(
        items: &'a [(&'a str, &'a str, PackedRgba)],
        separator_color: PackedRgba,
    ) -> Self {
        Self {
            items,
            separator_color,
        }
    }
}

impl Widget for SummaryFooter<'_> {
    fn render(&self, area: Rect, frame: &mut ftui::Frame) {
        if area.width < 5 || area.height == 0 || self.items.is_empty() {
            return;
        }

        let sep_style = Style::default().fg(self.separator_color);
        let mut spans: Vec<Span<'_>> = Vec::with_capacity(self.items.len() * 3);

        for (i, (value, label, color)) in self.items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" \u{2502} ", sep_style));
            }
            spans.push(Span::styled(
                format!("{value} {label}"),
                Style::default().fg(*color),
            ));
        }

        let line = Line::from_spans(spans);
        Paragraph::new(line).render(area, frame);
    }
}

/// Write a single-char cell with fg color at position (x, y).
///
/// Utility for widgets that need direct buffer access.
#[allow(dead_code)]
pub fn set_colored_char(frame: &mut ftui::Frame, x: u16, y: u16, ch: char, fg: PackedRgba) {
    let mut cell = Cell::from_char(ch);
    cell.fg = fg;
    frame.buffer.set_fast(x, y, cell);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_badge_renders_without_panic() {
        let badge = StatusBadge::new("\u{25CF}", "ACTIVE").style(Style::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 1, &mut pool);
        badge.render(Rect::new(0, 0, 40, 1), &mut frame);
    }

    #[test]
    fn status_badge_to_string() {
        let badge = StatusBadge::new("\u{25CF}", "ACTIVE");
        assert_eq!(badge.to_string_repr(), "\u{25CF} ACTIVE");
    }

    #[test]
    fn status_badge_tiny_area() {
        let badge = StatusBadge::new("\u{25CF}", "ACTIVE");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(2, 1, &mut pool);
        badge.render(Rect::new(0, 0, 2, 1), &mut frame);
    }

    #[test]
    fn summary_footer_renders_without_panic() {
        let items: Vec<(&str, &str, PackedRgba)> = vec![
            ("6", "projects", PackedRgba::rgb(50, 200, 200)),
            ("18", "agents", PackedRgba::rgb(255, 50, 150)),
            ("892", "msgs", PackedRgba::rgb(200, 50, 255)),
        ];
        let footer = SummaryFooter::new(&items, PackedRgba::rgb(100, 100, 100));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 1, &mut pool);
        footer.render(Rect::new(0, 0, 80, 1), &mut frame);
    }

    #[test]
    fn summary_footer_empty_items() {
        let items: Vec<(&str, &str, PackedRgba)> = vec![];
        let footer = SummaryFooter::new(&items, PackedRgba::rgb(100, 100, 100));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 1, &mut pool);
        footer.render(Rect::new(0, 0, 40, 1), &mut frame);
    }

    #[test]
    fn summary_footer_tiny_area() {
        let items: Vec<(&str, &str, PackedRgba)> =
            vec![("6", "projects", PackedRgba::rgb(50, 200, 200))];
        let footer = SummaryFooter::new(&items, PackedRgba::rgb(100, 100, 100));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(4, 1, &mut pool);
        footer.render(Rect::new(0, 0, 4, 1), &mut frame);
    }
}
