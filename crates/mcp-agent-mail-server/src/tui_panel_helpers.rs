//! Shared TUI panel helper functions.
//!
//! Reusable building blocks extracted from Dashboard patterns:
//! bordered panels and empty-state cards.

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Frame, Style};

use crate::tui_theme::TuiThemePalette;

// ──────────────────────────────────────────────────────────────────────
// Panel blocks
// ──────────────────────────────────────────────────────────────────────

/// Standard bordered panel with rounded corners and title.
///
/// Uses the current theme palette for border and background colors.
#[must_use]
pub fn panel_block(title: &str) -> Block<'_> {
    let tp = TuiThemePalette::current();
    Block::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
}

// ──────────────────────────────────────────────────────────────────────
// Empty state rendering
// ──────────────────────────────────────────────────────────────────────

/// Render a "no data" empty state card centered in the given area.
///
/// Shows an icon, title, and hint text centered vertically and
/// horizontally, wrapped in a rounded bordered panel.
pub fn render_empty_state(frame: &mut Frame<'_>, area: Rect, icon: &str, title: &str, hint: &str) {
    if area.height < 5 || area.width < 20 {
        // Too small for the card — just render a one-line fallback
        let tp = TuiThemePalette::current();
        let msg = format!("{icon} {title}");
        Paragraph::new(msg)
            .style(Style::default().fg(tp.text_muted))
            .render(area, frame);
        return;
    }
    let tp = TuiThemePalette::current();

    // Center a card of fixed size within the area
    let card_w = area.width.min(60);
    let card_h = area.height.min(7);
    let cx = area.x + (area.width.saturating_sub(card_w)) / 2;
    let cy = area.y + (area.height.saturating_sub(card_h)) / 2;
    let card_area = Rect::new(cx, cy, card_w, card_h);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg));

    let inner = block.inner(card_area);
    block.render(card_area, frame);

    // Icon + title on first line, hint on third line
    if inner.height >= 1 {
        let title_line = format!("{icon}  {title}");
        Paragraph::new(title_line)
            .style(Style::default().fg(tp.text_primary).bold())
            .render(Rect::new(inner.x, inner.y, inner.width, 1), frame);
    }
    if inner.height >= 3 {
        Paragraph::new(hint)
            .style(Style::default().fg(tp.text_muted))
            .render(
                Rect::new(
                    inner.x,
                    inner.y + 2,
                    inner.width,
                    inner.height.saturating_sub(2),
                ),
                frame,
            );
    }
}
