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

#[cfg(test)]
mod tests {
    use super::*;
    use ftui::layout::Rect;
    use ftui::GraphemePool;
    use ftui_extras::theme::{ScopedThemeLock, ThemeId};

    #[test]
    fn panel_block_returns_bordered_block_with_title() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let block = panel_block("Test Panel");
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(40, 10, &mut pool);
        block.render(Rect::new(0, 0, 40, 10), &mut frame);
    }

    #[test]
    fn panel_block_works_across_all_themes() {
        let themes = [
            ThemeId::CyberpunkAurora,
            ThemeId::Darcula,
            ThemeId::LumenLight,
            ThemeId::NordicFrost,
            ThemeId::Doom,
        ];
        for &theme in &themes {
            let _guard = ScopedThemeLock::new(theme);
            let block = panel_block("Themed");
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(30, 8, &mut pool);
            block.render(Rect::new(0, 0, 30, 8), &mut frame);
        }
    }

    #[test]
    fn render_empty_state_normal_area() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 80, 24), "M", "No Messages", "Send a message to get started");
    }

    #[test]
    fn render_empty_state_small_area_fallback() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(15, 3, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 15, 3), "M", "Empty", "hint text");
    }

    #[test]
    fn render_empty_state_minimum_width_boundary() {
        let _guard = ScopedThemeLock::new(ThemeId::Darcula);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 20, 5), "S", "No Results", "Try a different query");
    }

    #[test]
    fn render_empty_state_just_below_width_threshold() {
        let _guard = ScopedThemeLock::new(ThemeId::Darcula);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(19, 5, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 19, 5), "S", "No Results", "hint");
    }

    #[test]
    fn render_empty_state_just_below_height_threshold() {
        let _guard = ScopedThemeLock::new(ThemeId::NordicFrost);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(80, 4, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 80, 4), "!", "Warning", "something");
    }

    #[test]
    fn render_empty_state_card_clamped_to_60_wide() {
        let _guard = ScopedThemeLock::new(ThemeId::LumenLight);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(200, 50, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 200, 50), "D", "No Data", "Data will appear here");
    }

    #[test]
    fn render_empty_state_with_offset_area() {
        let _guard = ScopedThemeLock::new(ThemeId::Doom);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(100, 40, &mut pool);
        render_empty_state(&mut frame, Rect::new(10, 5, 60, 20), "I", "No Items", "Create an item to begin");
    }

    #[test]
    fn render_empty_state_zero_area() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_empty_state(&mut frame, Rect::new(0, 0, 0, 0), "M", "Empty", "hint");
    }

    #[test]
    fn panel_block_with_empty_title() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let block = panel_block("");
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(40, 10, &mut pool);
        block.render(Rect::new(0, 0, 40, 10), &mut frame);
    }

    #[test]
    fn panel_block_with_long_title() {
        let _guard = ScopedThemeLock::new(ThemeId::CyberpunkAurora);
        let long_title = "A".repeat(200);
        let block = panel_block(&long_title);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(40, 10, &mut pool);
        block.render(Rect::new(0, 0, 40, 10), &mut frame);
    }
}
