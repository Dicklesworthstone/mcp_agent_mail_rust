//! Markdown rendering + HTML sanitization for the Mail SSR UI.
//!
//! Legacy python uses `markdown2` (GFM-ish) plus `bleach` allowlists.
//! Here we use `comrak` for markdown rendering and `ammonia` for sanitization,
//! configured to match the legacy allowlists as closely as possible.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::Builder;
use comrak::{Options, markdown_to_html};

static COMRAK_OPTIONS: LazyLock<Options<'static>> = LazyLock::new(|| {
    let mut opts = Options::default();

    // Match legacy `markdown2` extras:
    // - fenced-code-blocks
    // - tables
    // - strike
    // - cuddled-lists (comrak handles this reasonably; no direct flag)
    opts.extension.table = true;
    opts.extension.strikethrough = true;

    // Legacy allows embedded HTML then sanitizes it (bleach). We do the same:
    // render HTML, then pass through the sanitizer.
    opts.render.r#unsafe = true;

    // Closer to the legacy UI behavior (and the templates' client-side marked config).
    opts.render.hardbreaks = true;

    opts
});

static HTML_SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut b = Builder::new();
    // Legacy python does not force rel on links; it merely allowlists it.
    // (Ammonia defaults to adding `rel="noopener noreferrer"`; disable to match legacy.)
    b.link_rel(None);

    // Align with legacy python allowlists.
    b.tags(
        [
            "a",
            "abbr",
            "acronym",
            "b",
            "blockquote",
            "code",
            "del", // Comrak uses <del> for ~~strikethrough~~.
            "em",
            "i",
            "li",
            "ol",
            "ul",
            "p",
            "pre",
            "strong",
            "table",
            "thead",
            "tbody",
            "tr",
            "th",
            "td",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "hr",
            "br",
            "span",
            "img",
        ]
        .into_iter()
        .collect::<HashSet<&'static str>>(),
    );

    // Equivalent to bleach `strip=True`.
    b.clean_content_tags(["script", "style"].into_iter().collect::<HashSet<_>>());

    // Allow CSS classes everywhere (Tailwind-heavy templates rely on this).
    b.add_generic_attributes(&["class"]);

    // Tag-specific attributes (matches python config).
    b.add_tag_attributes("a", &["href", "title", "rel"]);
    b.add_tag_attributes("abbr", &["title"]);
    b.add_tag_attributes("acronym", &["title"]);
    b.add_tag_attributes("code", &["class"]);
    b.add_tag_attributes("pre", &["class"]);

    b.add_tag_attributes("span", &["class", "style"]);
    b.add_tag_attributes("p", &["class", "style"]);
    b.add_tag_attributes("table", &["class", "style"]);
    b.add_tag_attributes("td", &["class", "style"]);
    b.add_tag_attributes("th", &["class", "style"]);

    b.add_tag_attributes(
        "img",
        &[
            "src", "alt", "title", "width", "height", "loading", "decoding", "class",
        ],
    );

    // Allowed URL schemes.
    b.url_schemes(
        ["http", "https", "mailto", "data"]
            .into_iter()
            .collect::<HashSet<_>>(),
    );

    // Only allow a small set of style properties (legacy python uses bleach CSSSanitizer).
    b.filter_style_properties(
        [
            "color",
            "background-color",
            "text-align",
            "text-decoration",
            "font-weight",
        ]
        .into_iter()
        .collect::<HashSet<_>>(),
    );

    b
});

pub fn render_markdown_to_safe_html(markdown: &str) -> String {
    if markdown.trim().is_empty() {
        return String::new();
    }

    let html = markdown_to_html(markdown, &COMRAK_OPTIONS);
    HTML_SANITIZER.clean(&html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(render_markdown_to_safe_html(""), "");
        assert_eq!(render_markdown_to_safe_html("   "), "");
        assert_eq!(render_markdown_to_safe_html("\n\n"), "");
    }

    #[test]
    fn basic_paragraph() {
        let html = render_markdown_to_safe_html("Hello world");
        assert!(html.contains("<p>"));
        assert!(html.contains("Hello world"));
    }

    #[test]
    fn bold_and_italic() {
        let html = render_markdown_to_safe_html("**bold** and *italic*");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>italic</em>"));
    }

    #[test]
    fn fenced_code_block() {
        let html = render_markdown_to_safe_html("```rust\nfn main() {}\n```");
        assert!(html.contains("<code"));
        assert!(html.contains("fn main()"));
    }

    #[test]
    fn table_rendering() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let html = render_markdown_to_safe_html(md);
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>"));
    }

    #[test]
    fn strikethrough() {
        let html = render_markdown_to_safe_html("~~deleted~~");
        assert!(html.contains("<del>deleted</del>"));
    }

    #[test]
    fn links_preserved() {
        let html = render_markdown_to_safe_html("[click](https://example.com)");
        assert!(html.contains("<a"));
        assert!(html.contains("href=\"https://example.com\""));
    }

    #[test]
    fn images_preserved() {
        let html = render_markdown_to_safe_html("![alt](https://example.com/img.png)");
        assert!(html.contains("<img"));
        assert!(html.contains("src=\"https://example.com/img.png\""));
    }

    #[test]
    fn script_tags_stripped() {
        let html = render_markdown_to_safe_html("<script>alert('xss')</script>");
        assert!(!html.contains("<script>"));
        assert!(!html.contains("alert"));
    }

    #[test]
    fn style_tags_stripped() {
        let html = render_markdown_to_safe_html(&format!("<style>body{}</style>", "{color:red}"));
        assert!(!html.contains("<style>"));
    }

    #[test]
    fn onclick_stripped() {
        let html = render_markdown_to_safe_html("<a onclick=\"alert(1)\" href=\"#\">x</a>");
        assert!(!html.contains("onclick"));
        assert!(html.contains("<a"));
    }

    #[test]
    fn javascript_url_stripped() {
        let html = render_markdown_to_safe_html("<a href=\"javascript:alert(1)\">bad</a>");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn allowed_style_properties() {
        let html =
            render_markdown_to_safe_html("<span style=\"color:red;text-align:center\">ok</span>");
        assert!(html.contains("color:red"));
    }

    #[test]
    fn disallowed_style_properties_stripped() {
        let html =
            render_markdown_to_safe_html("<span style=\"position:absolute;top:0\">bad</span>");
        assert!(!html.contains("position"));
    }

    #[test]
    fn class_attribute_allowed() {
        let html = render_markdown_to_safe_html("<p class=\"foo\">text</p>");
        assert!(html.contains("class=\"foo\""));
    }

    #[test]
    fn unordered_list() {
        let html = render_markdown_to_safe_html("- one\n- two\n- three");
        assert!(html.contains("<ul>"));
        assert!(html.contains("<li>"));
    }

    #[test]
    fn ordered_list() {
        let html = render_markdown_to_safe_html("1. one\n2. two");
        assert!(html.contains("<ol>"));
        assert!(html.contains("<li>"));
    }

    #[test]
    fn headings() {
        let html = render_markdown_to_safe_html("# H1\n## H2\n### H3");
        assert!(html.contains("<h1>"));
        assert!(html.contains("<h2>"));
        assert!(html.contains("<h3>"));
    }

    #[test]
    fn blockquote() {
        let html = render_markdown_to_safe_html("> quoted text");
        assert!(html.contains("<blockquote>"));
    }

    #[test]
    fn data_uri_images_allowed() {
        let html =
            render_markdown_to_safe_html("<img src=\"data:image/png;base64,abc123\" alt=\"pic\">");
        assert!(html.contains("data:image/png"));
    }

    // --- Parity conformance tests (br-3vwi.13.3) ---

    #[test]
    fn code_block_with_language_class_preserved() {
        let html = render_markdown_to_safe_html("```python\nprint('hello')\n```");
        assert!(html.contains("<pre>"));
        assert!(html.contains("<code"));
        assert!(html.contains("language-python"));
        assert!(html.contains("print(&#x27;hello&#x27;)") || html.contains("print('hello')"));
    }

    #[test]
    fn nested_lists() {
        let md = "- outer\n  - inner\n  - inner2\n- outer2";
        let html = render_markdown_to_safe_html(md);
        assert!(html.contains("<ul>"));
        assert!(html.contains("outer"));
        assert!(html.contains("inner"));
    }

    #[test]
    fn mixed_html_and_markdown() {
        let md = "**bold** and <em>html italic</em> and `code`";
        let html = render_markdown_to_safe_html(md);
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>html italic</em>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn inline_code_not_rendered() {
        let html = render_markdown_to_safe_html("`**not bold**`");
        assert!(html.contains("<code>**not bold**</code>") || html.contains("<code>"));
        assert!(!html.contains("<strong>"));
    }

    #[test]
    fn link_no_rel_attribute_forced() {
        // Python parity: bleach does not force rel on links.
        let html = render_markdown_to_safe_html("[link](https://example.com)");
        assert!(!html.contains("noopener"));
        assert!(!html.contains("noreferrer"));
    }

    #[test]
    fn table_with_alignment() {
        let md = "| Left | Center | Right |\n|:-----|:------:|------:|\n| a | b | c |";
        let html = render_markdown_to_safe_html(md);
        assert!(html.contains("<table>"));
        assert!(html.contains("<th"));
        assert!(html.contains("<td"));
    }

    #[test]
    fn horizontal_rule() {
        let html = render_markdown_to_safe_html("above\n\n---\n\nbelow");
        assert!(html.contains("<hr"));
    }

    #[test]
    fn hard_breaks_enabled() {
        let html = render_markdown_to_safe_html("line one\nline two");
        assert!(html.contains("<br"));
    }

    #[test]
    fn svg_xss_stripped() {
        let html =
            render_markdown_to_safe_html("<svg onload=\"alert(1)\"><circle r=10></circle></svg>");
        assert!(!html.contains("<svg"));
        assert!(!html.contains("onload"));
    }

    #[test]
    fn img_onerror_stripped() {
        let html = render_markdown_to_safe_html("<img src=x onerror=\"alert(1)\">");
        assert!(!html.contains("onerror"));
    }

    #[test]
    fn encoded_javascript_url_stripped() {
        let html = render_markdown_to_safe_html("<a href=\"&#106;avascript:alert(1)\">click</a>");
        assert!(!html.contains("javascript"));
    }

    #[test]
    fn unicode_preserved() {
        let html = render_markdown_to_safe_html("日本語テスト 🦀 Ñoño");
        assert!(html.contains("日本語テスト"));
        assert!(html.contains("🦀"));
        assert!(html.contains("Ñoño"));
    }

    #[test]
    fn long_content_does_not_truncate() {
        let long = "x".repeat(100_000);
        let html = render_markdown_to_safe_html(&long);
        assert!(html.len() >= 100_000);
    }
}
