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

    // ── Core GFM features (br-3vwi.3.2) ──────────────────────────

    #[test]
    fn heading_levels_render() {
        let md = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6";
        let text = render_body(md, &theme());
        assert!(text.height() >= 6, "All 6 heading levels should render");
    }

    #[test]
    fn unordered_list_renders() {
        let md = "- first\n- second\n- third";
        let text = render_body(md, &theme());
        assert!(text.height() >= 3);
    }

    #[test]
    fn ordered_list_renders() {
        let md = "1. first\n2. second\n3. third";
        let text = render_body(md, &theme());
        assert!(text.height() >= 3);
    }

    #[test]
    fn nested_list_renders() {
        let md = "- parent\n  - child\n    - grandchild\n- sibling";
        let text = render_body(md, &theme());
        assert!(text.height() >= 4);
    }

    #[test]
    fn inline_code_renders() {
        let md = "Use `cargo test` to run tests.";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn bold_and_italic_render() {
        let md = "**bold** and *italic* and ***both***";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn strikethrough_renders() {
        let md = "~~deleted~~ and **kept**";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn link_renders() {
        let md = "[click here](https://example.com) for more";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn thematic_break_renders() {
        let md = "above\n\n---\n\nbelow";
        let text = render_body(md, &theme());
        // Should have: above, blank, rule, blank, below (at least 3 lines)
        assert!(text.height() >= 3);
    }

    #[test]
    fn code_fence_with_language_renders() {
        let md = "```python\ndef greet(name):\n    print(f'Hello {name}')\n```";
        let text = render_body(md, &theme());
        assert!(text.height() >= 2);
    }

    #[test]
    fn gfm_table_multirow_renders() {
        let md = "\
| Name | Age | City |
|------|-----|------|
| Alice | 30 | NYC |
| Bob | 25 | LA |
| Carol | 35 | CHI |";
        let text = render_body(md, &theme());
        assert!(text.height() >= 4, "Table should have header + 3 data rows");
    }

    #[test]
    fn nested_blockquote_renders() {
        let md = "> level 1\n>> level 2\n>>> level 3";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn mixed_content_realistic_message() {
        let md = "\
# Status Update

Hello team,

Here are today's tasks:

1. **Fix** the login bug
2. Review PR `#123`
3. Deploy to staging

> Note: the deadline is ~~Friday~~ **Monday**

```rust
fn main() {
    println!(\"deployed!\");
}
```

| Task | Status |
|------|--------|
| Login fix | Done |
| PR review | Pending |

---

Thanks!";
        let text = render_body(md, &theme());
        // A realistic multi-element message should render many lines
        assert!(
            text.height() >= 15,
            "Realistic message should produce 15+ lines, got {}",
            text.height()
        );
    }

    #[test]
    fn footnote_renders() {
        let md = "See the docs[^1] for details.\n\n[^1]: Documentation link";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn render_body_preserves_multiline() {
        let md = "line one\n\nline three\n\nline five";
        let text = render_body(md, &theme());
        // Plain text with blank lines should preserve structure
        assert!(text.height() >= 3);
    }

    #[test]
    fn streaming_incomplete_bold() {
        let partial = "Some **bold text without closing";
        let text = render_body_streaming(partial, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn streaming_incomplete_list() {
        let partial = "- item one\n- item two\n- item";
        let text = render_body_streaming(partial, &theme());
        assert!(text.height() >= 3);
    }
}
