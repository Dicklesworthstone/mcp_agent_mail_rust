//! Markdown-to-terminal rendering for mail message bodies.
//!
//! Wraps [`ftui_extras::markdown`] to provide GFM rendering with
//! auto-detection: if text looks like markdown it's rendered with full
//! styling, otherwise it's displayed as plain text.

use ftui::text::Text;
pub use ftui_extras::markdown::{MarkdownRenderer, MarkdownTheme, is_likely_markdown};

/// Render a message body with auto-detected markdown support.
///
/// If the text appears to contain GFM formatting (headings, bold,
/// code fences, lists, tables, etc.) it is rendered through the full
/// markdown pipeline with syntax highlighting. Otherwise it is returned
/// as plain unstyled text.
#[must_use]
pub fn render_body(body: &str, theme: &MarkdownTheme) -> Text {
    let renderer = MarkdownRenderer::new(theme.clone());
    renderer.auto_render(body)
}

/// Render a potentially incomplete/streaming message body.
///
/// Same as [`render_body`] but closes unclosed fences, bold markers,
/// etc. before parsing so partial content renders gracefully.
#[must_use]
pub fn render_body_streaming(body: &str, theme: &MarkdownTheme) -> Text {
    let renderer = MarkdownRenderer::new(theme.clone());
    renderer.auto_render_streaming(body)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> MarkdownTheme {
        MarkdownTheme::default()
    }

    #[test]
    fn plain_text_passes_through() {
        let text = render_body("hello world", &theme());
        assert!(text.height() > 0);
        // Plain text should have one line
        assert_eq!(text.height(), 1);
    }

    #[test]
    fn markdown_heading_renders() {
        let text = render_body("# Hello\n\nSome **bold** text.", &theme());
        // Markdown produces more lines (heading + blank + body)
        assert!(text.height() >= 2);
    }

    #[test]
    fn code_fence_renders() {
        let body = "```rust\nfn main() {}\n```";
        let text = render_body(body, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn auto_detect_plain_stays_plain() {
        let plain = "just a regular message with no formatting";
        let detection = is_likely_markdown(plain);
        assert!(!detection.is_likely());
    }

    #[test]
    fn auto_detect_markdown_detected() {
        let md = "# Title\n\n- item **one**\n- item two";
        let detection = is_likely_markdown(md);
        assert!(detection.is_likely());
    }

    #[test]
    fn streaming_closes_open_fence() {
        let partial = "```python\ndef foo():\n    pass";
        let text = render_body_streaming(partial, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn empty_body_renders_empty() {
        let text = render_body("", &theme());
        assert_eq!(text.height(), 0);
    }

    #[test]
    fn gfm_table_renders() {
        let table = "| A | B |\n|---|---|\n| 1 | 2 |";
        let text = render_body(table, &theme());
        assert!(text.height() >= 2);
    }

    #[test]
    fn task_list_renders() {
        let md = "- [x] done\n- [ ] pending";
        let text = render_body(md, &theme());
        assert!(text.height() >= 2);
    }

    #[test]
    fn blockquote_renders() {
        let md = "> Some **quoted** text\n> with continuation";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }
}
