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
