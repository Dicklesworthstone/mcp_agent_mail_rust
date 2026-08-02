//! Minimal browser-safe log-pane adapter used by the shared dashboard.

use ftui::layout::Rect;
use ftui::text::{Line, Text};
use ftui::widgets::log_viewer::{LogViewer, LogViewerState, LogWrapMode};
use ftui_widgets::StatefulWidget;

const LOG_PANE_MAX_LINES: usize = 5_000;

/// Parse a console line for display in the browser dashboard.
///
/// Public demo packs are plain text and have already passed the privacy gate.
/// ANSI control bytes are removed defensively rather than interpreted.
#[must_use]
pub fn ansi_to_line(input: &str) -> Line<'static> {
    let mut plain = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            plain.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            let _ = chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    Line::raw(plain)
}

/// Browser-safe wrapper around FrankenTUI's virtualized log viewer.
pub struct LogPane {
    viewer: LogViewer,
    state: LogViewerState,
}

impl LogPane {
    #[must_use]
    pub fn new() -> Self {
        Self {
            viewer: LogViewer::new(LOG_PANE_MAX_LINES).wrap_mode(LogWrapMode::CharWrap),
            state: LogViewerState::default(),
        }
    }

    pub fn push<'a>(&mut self, line: impl Into<Text<'a>>) {
        self.viewer.push(line);
    }

    pub fn push_many<'a>(&mut self, lines: impl IntoIterator<Item = impl Into<Text<'a>>>) {
        self.viewer.push_many(lines);
    }

    pub fn clear(&mut self) {
        self.viewer.clear();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.viewer.scroll_to_bottom();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.viewer.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.viewer.is_empty()
    }

    pub fn render(&mut self, area: Rect, frame: &mut ftui::Frame<'_>) {
        self.viewer.render(area, frame, &mut self.state);
    }
}

impl Default for LogPane {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ansi_to_line;

    #[test]
    fn ansi_is_removed_from_public_console_lines() {
        let line = ansi_to_line("\u{1b}[31merror\u{1b}[0m");
        assert_eq!(line.spans()[0].content.as_ref(), "error");
    }
}
