//! Markdown-to-terminal rendering for mail message bodies.
//!
//! Wraps [`ftui_extras::markdown`] to provide GFM rendering with
//! auto-detection: if text looks like markdown it's rendered with full
//! styling, otherwise it's displayed as plain text.

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::Builder;
use ftui::text::Text;
pub use ftui_extras::markdown::{MarkdownRenderer, MarkdownTheme, is_likely_markdown};

/// Sanitizer for hostile inline HTML embedded in markdown message bodies.
///
/// This keeps markdown syntax intact while stripping dangerous HTML/script
/// payloads and disallowed URL schemes before terminal rendering.
static TERMINAL_MARKDOWN_SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::new();
    builder.clean_content_tags(["script", "style"].into_iter().collect::<HashSet<_>>());
    builder.url_schemes(
        ["http", "https", "mailto", "data"]
            .into_iter()
            .collect::<HashSet<_>>(),
    );
    builder
});

#[must_use]
fn sanitize_body(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    TERMINAL_MARKDOWN_SANITIZER.clean(body).to_string()
}

/// Render a message body with auto-detected markdown support.
///
/// If the text appears to contain GFM formatting (headings, bold,
/// code fences, lists, tables, etc.) it is rendered through the full
/// markdown pipeline with syntax highlighting. Otherwise it is returned
/// as plain unstyled text.
#[must_use]
pub fn render_body(body: &str, theme: &MarkdownTheme) -> Text {
    let renderer = MarkdownRenderer::new(theme.clone());
    let sanitized = sanitize_body(body);
    renderer.auto_render(&sanitized)
}

/// Render a potentially incomplete/streaming message body.
///
/// Same as [`render_body`] but closes unclosed fences, bold markers,
/// etc. before parsing so partial content renders gracefully.
#[must_use]
pub fn render_body_streaming(body: &str, theme: &MarkdownTheme) -> Text {
    let renderer = MarkdownRenderer::new(theme.clone());
    let sanitized = sanitize_body(body);
    renderer.auto_render_streaming(&sanitized)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
    fn code_fence_priority_languages_render_content() {
        let cases = [
            ("json", "{ \"ok\": true, \"count\": 7 }", "count"),
            ("python", "def greet(name):\n    return name", "greet"),
            ("rust", "fn main() { println!(\"hi\"); }", "main"),
            ("javascript", "function hi() { return 1; }", "function"),
            ("bash", "echo hello-world", "hello-world"),
        ];

        for (lang, code, needle) in cases {
            let md = format!("```{lang}\n{code}\n```");
            let text = render_body(&md, &theme());
            let rendered = text_to_string(&text);
            assert!(
                rendered.contains(needle),
                "rendered output for {lang} should preserve {needle}"
            );
            assert!(
                text.height() >= 1,
                "rendered output for {lang} should not be empty"
            );
        }
    }

    #[test]
    fn code_fence_unknown_language_falls_back_without_losing_content() {
        let md = "```unknownlang\n++>---.\n```";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        assert!(
            rendered.contains("++>---."),
            "unknown language fence should preserve code content"
        );
        assert!(text.height() >= 1);
    }

    #[test]
    fn long_code_block_render_timing_diagnostic() {
        let code = (0..1000)
            .map(|i| format!("let v{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let md = format!("```rust\n{code}\n```");
        let started = Instant::now();
        let text = render_body(&md, &theme());
        let elapsed = started.elapsed();

        eprintln!(
            "scenario=md_long_code_block lines=1000 elapsed_ms={} height={}",
            elapsed.as_millis(),
            text.height()
        );

        assert!(
            text.height() >= 1000,
            "expected rendered code lines to remain visible"
        );
        assert!(
            elapsed.as_secs_f64() < 5.0,
            "unexpectedly slow long-code render: {:.3}s",
            elapsed.as_secs_f64()
        );
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

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn sanitize_body_strips_script_and_style_tags() {
        let dirty = "<script>alert('xss')</script><style>body{color:red}</style>ok";
        let cleaned = sanitize_body(dirty);
        assert!(!cleaned.to_lowercase().contains("<script"));
        assert!(!cleaned.to_lowercase().contains("<style"));
        assert!(cleaned.contains("ok"));
    }

    #[test]
    fn sanitize_body_blocks_javascript_urls() {
        let dirty = "<a href=\"javascript:alert(1)\">click</a>";
        let cleaned = sanitize_body(dirty);
        assert!(!cleaned.to_lowercase().contains("javascript:"));
    }

    #[test]
    fn sanitize_body_preserves_markdown_syntax() {
        let md = "# Title\n\n**bold** `code`";
        let cleaned = sanitize_body(md);
        assert!(cleaned.contains("# Title"));
        assert!(cleaned.contains("**bold**"));
        assert!(cleaned.contains("`code`"));
    }

    // ── Security / hostile markdown tests ─────────────────────────

    #[test]
    fn hostile_script_tag_safe_in_terminal() {
        // Scripts should be stripped by ammonia before terminal rendering.
        let md = "Hello <script>alert('xss')</script> world";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        assert!(rendered.contains("Hello"), "surrounding text preserved");
        assert!(rendered.contains("world"), "surrounding text preserved");
        assert!(
            !rendered.to_lowercase().contains("script"),
            "script tag should be removed"
        );
        assert!(text.height() >= 1, "renders without panic");
    }

    #[test]
    fn hostile_onerror_safe_in_terminal() {
        // Event handlers are inert in terminal rendering — no DOM to attach to
        let md = "![img](x onerror=alert(1))";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1, "renders without panic");
    }

    #[test]
    fn hostile_javascript_url_safe_in_terminal() {
        // javascript: URLs are inert in terminal — no browser to execute
        let md = "[click](javascript:alert(1))";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        assert!(rendered.contains("click"), "link text preserved");
        assert!(text.height() >= 1, "renders without panic");
    }

    #[test]
    fn hostile_deeply_nested_markup() {
        // Deeply nested emphasis/bold shouldn't cause stack overflow
        let deep = "*".repeat(500) + "text" + &"*".repeat(500);
        let text = render_body(&deep, &theme());
        // Should render without panic — content doesn't matter
        assert!(text.height() >= 1);
    }

    #[test]
    fn hostile_huge_heading() {
        let md = format!("# {}\n\nBody", "A".repeat(10_000));
        let text = render_body(&md, &theme());
        assert!(text.height() >= 2);
    }

    #[test]
    fn hostile_huge_table() {
        // Table with many columns
        let header = (0..100)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join("|");
        let sep = (0..100).map(|_| "---").collect::<Vec<_>>().join("|");
        let row = (0..100)
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join("|");
        let md = format!("|{header}|\n|{sep}|\n|{row}|");
        let text = render_body(&md, &theme());
        assert!(
            text.height() >= 1,
            "Large table should render without panic"
        );
    }

    #[test]
    fn hostile_unclosed_code_fence() {
        let md = "```\nunclosed code\nblock\nhere";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn hostile_zero_width_characters() {
        let md = "Hello\u{200B}World\u{200B}Test **bold\u{200B}text**";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn hostile_control_characters() {
        let md = "Hello\x01\x02\x03World\n**bold\x0B text**";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn hostile_ansi_escape_in_markdown() {
        // ANSI escape sequences embedded in markdown content should render
        // without crashing. Terminal rendering uses styled spans (not raw ANSI),
        // so embedded escapes are treated as literal characters.
        let md = "Hello \x1b[31mred\x1b[0m text";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1, "renders without panic");
        let rendered = text_to_string(&text);
        // Core text should be preserved
        assert!(rendered.contains("Hello"), "surrounding text preserved");
        assert!(rendered.contains("text"), "surrounding text preserved");
    }

    #[test]
    fn hostile_null_bytes() {
        let md = "Hello\0World\0**bold**";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1);
    }

    #[test]
    fn hostile_extremely_long_line() {
        let long_line = "x".repeat(100_000);
        let md = format!("Start\n\n{long_line}\n\nEnd");
        let text = render_body(&md, &theme());
        assert!(text.height() >= 1, "Extremely long line should not panic");
    }

    #[test]
    fn hostile_many_backticks() {
        // Many backtick sequences that could confuse fence detection
        let md = "````````````````````````````````";
        let text = render_body(md, &theme());
        assert!(text.height() >= 1, "should render at least one line");
    }

    #[test]
    fn hostile_html_entities() {
        let md = "Hello &lt;script&gt;alert(1)&lt;/script&gt; world";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        // Entities should decode safely, not execute
        assert!(
            !rendered.contains("<script"),
            "HTML entities must not become tags"
        );
    }

    #[test]
    fn hostile_image_with_huge_alt() {
        let alt = "A".repeat(50_000);
        let md = format!("![{alt}](https://example.com/img.png)");
        let text = render_body(&md, &theme());
        assert!(text.height() >= 1);
    }

    // ── Snapshot-style rendering consistency tests ─────────────────

    #[test]
    fn snapshot_heading_produces_styled_output() {
        let md = "# Title\n\nParagraph **text**.";
        let text = render_body(md, &theme());
        let lines = text.lines();
        // First line should be the heading
        assert!(!lines.is_empty());
        // Heading line should have some styled spans (not just raw text)
        let first = &lines[0];
        assert!(!first.spans().is_empty(), "heading should have spans");
    }

    #[test]
    fn snapshot_code_fence_has_content() {
        let md = "```\nhello\nworld\n```";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        assert!(
            rendered.contains("hello"),
            "code content should be preserved"
        );
        assert!(
            rendered.contains("world"),
            "code content should be preserved"
        );
    }

    #[test]
    fn snapshot_list_items_have_bullets_or_numbers() {
        let md = "- alpha\n- beta\n- gamma";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        // List items should contain their text
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("beta"));
        assert!(rendered.contains("gamma"));
    }

    // ── Helper ────────────────────────────────────────────────────

    /// Flatten styled Text into a plain string for assertion checks.
    fn text_to_string(text: &Text) -> String {
        text.lines()
            .iter()
            .map(|line| {
                line.spans()
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
