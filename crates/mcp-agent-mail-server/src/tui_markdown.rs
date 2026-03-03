//! Markdown-to-terminal rendering for mail message bodies.
//!
//! Wraps [`ftui_extras::markdown`] to provide GFM rendering with
//! auto-detection: if text looks like markdown it's rendered with full
//! styling, otherwise it's displayed as plain text.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

use ftui::PackedRgba;
use ftui::text::{Line, Span};
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
pub fn render_body(body: &str, theme: &MarkdownTheme) -> Text<'static> {
    let renderer = MarkdownRenderer::new(theme.clone());
    let sanitized = sanitize_body(body);
    renderer.auto_render(&sanitized)
}

/// Render a potentially incomplete/streaming message body.
///
/// Same as [`render_body`] but closes unclosed fences, bold markers,
/// etc. before parsing so partial content renders gracefully.
#[must_use]
pub fn render_body_streaming(body: &str, theme: &MarkdownTheme) -> Text<'static> {
    let renderer = MarkdownRenderer::new(theme.clone());
    let sanitized = sanitize_body(body);
    renderer.auto_render_streaming(&sanitized)
}

// ──────────────────────────────────────────────────────────────────────
// Canonical Message-Body Rendering Contract (C1 / br-2k3qx.3.1)
// ──────────────────────────────────────────────────────────────────────
//
// All TUI surfaces that display message body content MUST use these
// functions to ensure consistent markdown detection, JSON wrapping,
// empty-body handling, sanitization, and truth-assertion semantics.

/// Maximum body preview length (characters) for list views and dashboards.
pub const BODY_PREVIEW_MAX_CHARS: usize = 200;

/// Render a message body through the canonical pipeline:
/// 1. Empty → returns `None` (caller decides placeholder)
/// 2. JSON auto-detection → wraps in json code fence
/// 3. Sanitize → strip scripts/style, limit URL schemes
/// 4. Markdown auto-detect → full GFM or plain text rendering
///
/// Returns `None` when the body is empty/whitespace-only. The caller
/// should render a placeholder like "(empty body)" in hint style.
#[must_use]
pub fn render_message_body(body_md: &str, theme: &MarkdownTheme) -> Option<Text<'static>> {
    if body_md.trim().is_empty() {
        return None;
    }
    let prepared = prepare_body_for_render(body_md);
    Some(render_body(&prepared, theme))
}

/// Render a truncated plain-text preview of a message body.
///
/// Useful for list views, dashboard previews, and table cells where
/// full markdown rendering is too expensive or visually noisy.
/// Returns `None` when the body is empty/whitespace-only.
#[must_use]
pub fn render_message_body_preview(body_md: &str, max_chars: usize) -> Option<String> {
    if body_md.trim().is_empty() {
        return None;
    }
    // Strip markdown syntax for preview: render then extract plain text.
    let theme = MarkdownTheme::default();
    let rendered = render_body(body_md, &theme);
    let plain = rendered
        .lines()
        .iter()
        .map(|line| {
            line.spans()
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(truncate_str(&trimmed, max_chars))
}

/// Render a message body as a blockquote preview (dashboard style).
///
/// Wraps the body excerpt in `> ` blockquote markers before rendering,
/// suitable for "recent message" preview cards.
/// Returns `None` when the body is empty/whitespace-only.
#[must_use]
pub fn render_message_body_blockquote(
    body_md: &str,
    theme: &MarkdownTheme,
) -> Option<Text<'static>> {
    if body_md.trim().is_empty() {
        return None;
    }
    let trimmed_excerpt = body_md.trim_end_matches('\n');
    let quoted = format!("> {}", trimmed_excerpt.replace('\n', "\n> "));
    Some(render_body(&quoted, theme))
}

/// Detect if a body string looks like raw JSON (object or array).
///
/// Returns `true` when the trimmed body starts with `{` or `[` and is
/// NOT already wrapped in a code fence. Used to auto-wrap JSON payloads
/// in json code fences for syntax highlighting.
#[must_use]
pub fn looks_like_json(body: &str) -> bool {
    let trimmed = body.trim_start();
    (trimmed.starts_with('{') || trimmed.starts_with('[')) && !trimmed.starts_with("```")
}

/// Prepare body for rendering: apply JSON auto-wrapping if needed.
///
/// This is the shared pre-processing step before `render_body()`.
#[must_use]
fn prepare_body_for_render(body_md: &str) -> String {
    if looks_like_json(body_md) {
        format!("```json\n{}\n```", body_md.trim_end())
    } else {
        body_md.to_string()
    }
}

/// One visible row in a collapsible JSON tree rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonTreeRow {
    /// Stable node path (JSON Pointer-like, rooted at `$`).
    pub path: String,
    /// Whether the node has children that can be expanded/collapsed.
    pub expandable: bool,
    /// Whether the node is currently expanded.
    pub expanded: bool,
    /// Human-readable scalar/summary value for this row.
    pub value_preview: String,
    /// Pre-styled render line.
    pub line: Line<'static>,
}

/// Shared JSON-tree interaction state for detail panes.
#[derive(Debug, Clone, Default)]
pub struct JsonTreeViewState {
    body_hash: Option<u64>,
    parsed_value: Option<serde_json::Value>,
    expanded_paths: HashSet<String>,
    cursor: usize,
}

impl JsonTreeViewState {
    /// Synchronize the state against a candidate body payload.
    ///
    /// Returns `true` when `body` is valid JSON and tree view is available.
    pub fn sync_body(&mut self, body: &str) -> bool {
        let body_hash = stable_hash(body.as_bytes());
        if self.body_hash == Some(body_hash) {
            return self.parsed_value.is_some();
        }

        self.body_hash = Some(body_hash);
        self.parsed_value = serde_json::from_str::<serde_json::Value>(body.trim()).ok();
        self.expanded_paths.clear();
        self.cursor = 0;
        if self.parsed_value.is_some() {
            self.expanded_paths.insert("$".to_string());
        }
        self.parsed_value.is_some()
    }

    /// Whether the currently synced payload is valid JSON.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.parsed_value.is_some()
    }

    /// Build flattened rows for the current JSON tree.
    #[must_use]
    pub fn rows(&self) -> Vec<JsonTreeRow> {
        let Some(value) = self.parsed_value.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        flatten_json_rows(value, None, "$".to_string(), 0, &self.expanded_paths, &mut out);
        out
    }

    /// Current selected row index.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move selection by `delta` rows.
    pub fn move_cursor_by(&mut self, delta: isize) {
        let rows_len = self.rows().len();
        if rows_len == 0 {
            self.cursor = 0;
            return;
        }
        if delta.is_negative() {
            self.cursor = self.cursor.saturating_sub(delta.unsigned_abs());
        } else {
            #[allow(clippy::cast_sign_loss)]
            let step = delta as usize;
            self.cursor = self.cursor.saturating_add(step).min(rows_len.saturating_sub(1));
        }
    }

    /// Toggle expand/collapse on the selected row.
    ///
    /// Returns `true` when a node expansion state changed.
    pub fn toggle_selected(&mut self) -> bool {
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor) else {
            self.cursor = 0;
            return false;
        };
        if !row.expandable {
            return false;
        }
        if row.expanded {
            self.expanded_paths.remove(&row.path);
        } else {
            self.expanded_paths.insert(row.path.clone());
        }
        true
    }

    /// Ensure the cursor remains within row bounds.
    pub fn clamp_cursor(&mut self) {
        let rows_len = self.rows().len();
        if rows_len == 0 {
            self.cursor = 0;
        } else if self.cursor >= rows_len {
            self.cursor = rows_len - 1;
        }
    }
}

fn stable_hash<T: Hash>(value: T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

const JSON_KEY_FG: PackedRgba = PackedRgba::rgb(17, 168, 205);
const JSON_STRING_FG: PackedRgba = PackedRgba::rgb(13, 188, 121);
const JSON_NUMBER_FG: PackedRgba = PackedRgba::rgb(229, 229, 16);
const JSON_BOOL_FG: PackedRgba = PackedRgba::rgb(188, 63, 188);
const JSON_NULL_FG: PackedRgba = PackedRgba::rgb(120, 120, 120);
const JSON_PUNCT_FG: PackedRgba = PackedRgba::rgb(180, 180, 190);

fn flatten_json_rows(
    value: &serde_json::Value,
    key_label: Option<String>,
    path: String,
    depth: usize,
    expanded_paths: &HashSet<String>,
    out: &mut Vec<JsonTreeRow>,
) {
    let expandable = match value {
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => false,
    };
    let expanded = !expandable || expanded_paths.contains(path.as_str());
    let value_preview = format_json_value_preview(value);
    let line = build_json_tree_line(depth, key_label.as_deref(), value, &value_preview, expandable, expanded);
    out.push(JsonTreeRow {
        path: path.clone(),
        expandable,
        expanded,
        value_preview,
        line,
    });

    if !expandable || !expanded {
        return;
    }

    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}/{}", json_pointer_escape(key));
                flatten_json_rows(
                    child,
                    Some(key.clone()),
                    child_path,
                    depth + 1,
                    expanded_paths,
                    out,
                );
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let child_path = format!("{path}/{index}");
                flatten_json_rows(
                    child,
                    Some(format!("[{index}]")),
                    child_path,
                    depth + 1,
                    expanded_paths,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn json_pointer_escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn format_json_value_preview(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let count = map.len();
            let suffix = if count == 1 { "" } else { "s" };
            format!("{{{count} key{suffix}}}")
        }
        serde_json::Value::Array(items) => {
            let count = items.len();
            let suffix = if count == 1 { "" } else { "s" };
            format!("[{count} item{suffix}]")
        }
        serde_json::Value::String(s) => format!("\"{}\"", truncate_str(s, 96)),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
    }
}

fn build_json_tree_line(
    depth: usize,
    key_label: Option<&str>,
    value: &serde_json::Value,
    value_preview: &str,
    expandable: bool,
    expanded: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if depth > 0 {
        spans.push(Span::raw("  ".repeat(depth)));
    }
    let marker = if expandable {
        if expanded { "▼ " } else { "▶ " }
    } else {
        "  "
    };
    spans.push(Span::styled(
        marker.to_string(),
        ftui::Style::default().fg(JSON_PUNCT_FG),
    ));
    if let Some(label) = key_label {
        let rendered_label = if label.starts_with('[') {
            label.to_string()
        } else {
            format!("\"{label}\"")
        };
        spans.push(Span::styled(
            rendered_label,
            ftui::Style::default().fg(JSON_KEY_FG),
        ));
        spans.push(Span::styled(
            ": ".to_string(),
            ftui::Style::default().fg(JSON_PUNCT_FG),
        ));
    } else {
        spans.push(Span::styled(
            "root: ".to_string(),
            ftui::Style::default().fg(JSON_PUNCT_FG),
        ));
    }

    let value_style = match value {
        serde_json::Value::String(_) => ftui::Style::default().fg(JSON_STRING_FG),
        serde_json::Value::Number(_) => ftui::Style::default().fg(JSON_NUMBER_FG),
        serde_json::Value::Bool(_) => ftui::Style::default().fg(JSON_BOOL_FG),
        serde_json::Value::Null => ftui::Style::default().fg(JSON_NULL_FG),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            ftui::Style::default().fg(JSON_PUNCT_FG)
        }
    };
    spans.push(Span::styled(value_preview.to_string(), value_style));
    Line::from_spans(spans)
}

/// Truncate a string to at most `max_chars` characters, appending "..."
/// if truncation occurred.
#[must_use]
fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else if max_chars <= 3 {
        // Not enough room for any content plus "..."; just take max_chars raw chars.
        s.chars().take(max_chars).collect()
    } else {
        let truncated: String = s.chars().take(max_chars - 3).collect();
        format!("{truncated}...")
    }
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
        let md = "```unknownlang\nfoo = bar(42)\n```";
        let text = render_body(md, &theme());
        let rendered = text_to_string(&text);
        assert!(
            rendered.contains("foo = bar(42)"),
            "unknown language fence should preserve code content: {rendered}"
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn snapshot_golden_regression_matrix() {
        struct Case {
            scenario_id: &'static str,
            markdown: &'static str,
            min_height: usize,
            required_fragments: &'static [&'static str],
        }

        let cases = [
            Case {
                scenario_id: "heading_bold",
                markdown: "# Title\n\nSome **bold** text.",
                min_height: 2,
                required_fragments: &["Title", "Some", "bold"],
            },
            Case {
                scenario_id: "json_code_fence",
                markdown: "```json\n{\"service\":\"mail\",\"enabled\":true}\n```",
                min_height: 1,
                required_fragments: &["service", "enabled"],
            },
            Case {
                scenario_id: "hostile_script_sanitized",
                markdown: "Before <script>alert('xss')</script> After",
                min_height: 1,
                required_fragments: &["Before", "After"],
            },
            Case {
                scenario_id: "nested_list",
                markdown: "- parent\n  - child\n    - grandchild",
                min_height: 3,
                required_fragments: &["parent", "child", "grandchild"],
            },
            // C5: Additional fidelity snapshots for full GFM coverage.
            Case {
                scenario_id: "gfm_table",
                markdown: "| Col A | Col B |\n|-------|-------|\n| val1  | val2  |",
                min_height: 2,
                required_fragments: &["Col A", "Col B", "val1", "val2"],
            },
            Case {
                scenario_id: "link_inline",
                markdown: "Visit [Docs](https://example.com) for details.",
                min_height: 1,
                required_fragments: &["Visit", "Docs", "details"],
            },
            Case {
                scenario_id: "blockquote_nested",
                markdown: "> outer\n>> inner\n\nafter",
                min_height: 2,
                required_fragments: &["outer", "inner", "after"],
            },
            Case {
                scenario_id: "task_list",
                markdown: "- [x] done\n- [ ] pending\n- [x] also done",
                min_height: 3,
                required_fragments: &["done", "pending", "also done"],
            },
            Case {
                scenario_id: "italic_and_bold",
                markdown: "*italic* and **bold** and ***both***",
                min_height: 1,
                required_fragments: &["italic", "bold", "both"],
            },
            Case {
                scenario_id: "inline_code",
                markdown: "Run `cargo test` to verify the build.",
                min_height: 1,
                required_fragments: &["Run", "cargo test", "verify"],
            },
            Case {
                scenario_id: "ordered_list",
                markdown: "1. first\n2. second\n3. third",
                min_height: 3,
                required_fragments: &["first", "second", "third"],
            },
            Case {
                scenario_id: "strikethrough",
                markdown: "~~removed~~ kept",
                min_height: 1,
                required_fragments: &["removed", "kept"],
            },
            Case {
                scenario_id: "thematic_break",
                markdown: "above\n\n---\n\nbelow",
                min_height: 2,
                required_fragments: &["above", "below"],
            },
            Case {
                scenario_id: "mixed_realistic_message",
                markdown: "## Status Update\n\n**Build passed.** See the [report](https://ci.example.com).\n\n- Test coverage: 95%\n- Lint: clean\n\n```\ncargo test --all\n```\n\n> Review notes: LGTM",
                min_height: 5,
                required_fragments: &[
                    "Status Update",
                    "Build passed",
                    "report",
                    "coverage",
                    "cargo test",
                    "LGTM",
                ],
            },
        ];

        for case in cases {
            let first = render_body(case.markdown, &theme());
            let second = render_body(case.markdown, &theme());
            let canonical = canonical_text(&first);
            let digest = stable_digest(&canonical);
            let second_digest = stable_digest(&canonical_text(&second));

            eprintln!(
                "scenario={} digest={} height={}",
                case.scenario_id,
                digest,
                first.height()
            );

            assert!(
                first.height() >= case.min_height,
                "{}: expected height >= {}, got {}",
                case.scenario_id,
                case.min_height,
                first.height()
            );
            for fragment in case.required_fragments {
                assert!(
                    canonical.contains(fragment),
                    "{}: rendered output missing fragment {:?}",
                    case.scenario_id,
                    fragment
                );
            }
            assert!(
                digest != 0,
                "{}: digest unexpectedly zero",
                case.scenario_id
            );
            assert_eq!(
                digest, second_digest,
                "{}: render digest should be deterministic",
                case.scenario_id
            );
        }
    }

    // ── Helper ────────────────────────────────────────────────────

    /// Canonicalize rendered text for deterministic snapshot hashing.
    fn canonical_text(text: &Text) -> String {
        text.lines()
            .iter()
            .map(|line| {
                line.spans()
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Stable FNV-1a digest for compact golden snapshot assertions.
    fn stable_digest(input: &str) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = FNV_OFFSET;
        for byte in input.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

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

    // ── Canonical Message-Body Contract Tests (C1 / br-2k3qx.3.1) ──

    #[test]
    fn render_message_body_returns_none_for_empty() {
        assert!(render_message_body("", &theme()).is_none());
        assert!(render_message_body("   ", &theme()).is_none());
        assert!(render_message_body("\n\n", &theme()).is_none());
    }

    #[test]
    fn render_message_body_renders_plain_text() {
        let result = render_message_body("Hello, world!", &theme());
        assert!(result.is_some());
        let text = result.unwrap();
        let rendered = text_to_string(&text);
        assert!(rendered.contains("Hello, world!"));
    }

    #[test]
    fn render_message_body_renders_markdown() {
        let result = render_message_body("# Title\n\n**bold** text", &theme());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.height() >= 2);
        let rendered = text_to_string(&text);
        assert!(rendered.contains("Title"));
        assert!(rendered.contains("bold"));
    }

    #[test]
    fn render_message_body_auto_wraps_json() {
        let json_body = r#"{"status":"ok","count":42}"#;
        let result = render_message_body(json_body, &theme());
        assert!(result.is_some());
        let text = result.unwrap();
        let rendered = text_to_string(&text);
        assert!(
            rendered.contains("status"),
            "JSON body should be rendered: {rendered}"
        );
    }

    #[test]
    fn render_message_body_auto_wraps_json_array() {
        let json_body = r#"[{"id":1},{"id":2}]"#;
        let result = render_message_body(json_body, &theme());
        assert!(result.is_some());
    }

    #[test]
    fn render_message_body_does_not_double_wrap_fenced_json() {
        let fenced = "```json\n{\"ok\":true}\n```";
        let result = render_message_body(fenced, &theme());
        assert!(result.is_some());
        let text = result.unwrap();
        let rendered = text_to_string(&text);
        assert!(rendered.contains("ok"));
    }

    #[test]
    fn render_message_body_preview_returns_none_for_empty() {
        assert!(render_message_body_preview("", 200).is_none());
        assert!(render_message_body_preview("  \n  ", 200).is_none());
    }

    #[test]
    fn render_message_body_preview_truncates_long_body() {
        let long_body = "word ".repeat(100);
        let preview = render_message_body_preview(&long_body, 50).unwrap();
        assert!(preview.len() <= 50, "preview should be at most 50 chars");
        assert!(
            preview.ends_with("..."),
            "truncated preview should end with ..."
        );
    }

    #[test]
    fn render_message_body_preview_preserves_short_body() {
        let short = "Hello there!";
        let preview = render_message_body_preview(short, 200).unwrap();
        assert_eq!(preview, "Hello there!");
    }

    #[test]
    fn render_message_body_preview_strips_markdown() {
        let md = "# Title\n\n**bold** text";
        let preview = render_message_body_preview(md, 200).unwrap();
        // Preview should be plain text, not raw markdown
        assert!(!preview.contains('#'));
        assert!(!preview.contains("**"));
        assert!(preview.contains("Title"));
        assert!(preview.contains("bold"));
    }

    #[test]
    fn render_message_body_blockquote_returns_none_for_empty() {
        assert!(render_message_body_blockquote("", &theme()).is_none());
        assert!(render_message_body_blockquote("   ", &theme()).is_none());
    }

    #[test]
    fn render_message_body_blockquote_wraps_content() {
        let body = "Hello\nWorld";
        let result = render_message_body_blockquote(body, &theme());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.height() >= 1);
    }

    #[test]
    fn looks_like_json_detects_objects() {
        assert!(looks_like_json(r#"{"key":"value"}"#));
        assert!(looks_like_json(r#"  {"key":"value"}"#));
    }

    #[test]
    fn looks_like_json_detects_arrays() {
        assert!(looks_like_json("[1,2,3]"));
        assert!(looks_like_json("  [1,2,3]"));
    }

    #[test]
    fn looks_like_json_rejects_non_json() {
        assert!(!looks_like_json("# heading"));
        assert!(!looks_like_json("plain text"));
        assert!(!looks_like_json("- list item"));
    }

    #[test]
    fn looks_like_json_rejects_already_fenced() {
        assert!(!looks_like_json("```json\n{}\n```"));
    }

    #[test]
    fn truncate_str_preserves_short_strings() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_truncates_long_strings() {
        let result = truncate_str("hello world foo bar", 10);
        assert!(result.chars().count() <= 10);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_str_handles_tiny_max_chars() {
        // max_chars < 3: no room for "..." suffix, just take raw chars.
        assert_eq!(truncate_str("hello", 2), "he");
        assert_eq!(truncate_str("hello", 1), "h");
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_boundary_at_three() {
        // max_chars == 3 with a long string: should return "..." or raw 3 chars.
        let result = truncate_str("hello", 3);
        assert!(result.chars().count() <= 3);
    }

    #[test]
    fn truncate_str_multibyte() {
        // Ensure char-level (not byte-level) truncation for non-ASCII.
        let result = truncate_str("\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F604}", 4);
        assert!(result.chars().count() <= 4);
    }

    #[test]
    fn render_message_body_sanitizes_hostile_content() {
        let hostile = "<script>alert('xss')</script>Safe content";
        let result = render_message_body(hostile, &theme());
        assert!(result.is_some());
        let rendered = text_to_string(&result.unwrap());
        assert!(!rendered.to_lowercase().contains("script"));
        assert!(rendered.contains("Safe content"));
    }

    #[test]
    fn render_message_body_handles_large_body() {
        let large = "x\n".repeat(5000);
        let result = render_message_body(&large, &theme());
        assert!(result.is_some());
        assert!(result.unwrap().height() >= 1000);
    }

    #[test]
    fn body_preview_max_chars_constant() {
        assert_eq!(BODY_PREVIEW_MAX_CHARS, 200);
    }

    // ── C5: Markdown fidelity regression fixtures ─────────────────

    /// Verify `render_message_body` preserves GFM table structure.
    #[test]
    fn c5_message_body_table_fidelity() {
        let md = "| Agent | Status |\n|-------|--------|\n| Blue  | Ready  |\n| Red   | Busy   |";
        let result = render_message_body(md, &theme());
        assert!(result.is_some(), "table body must render");
        let text = text_to_string(&result.unwrap());
        assert!(text.contains("Blue"), "table cell content preserved");
        assert!(text.contains("Ready"), "table cell content preserved");
        assert!(text.contains("Red"), "table cell content preserved");
    }

    /// Verify `render_message_body` preserves links and their text.
    #[test]
    fn c5_message_body_link_fidelity() {
        let md = "See the [deployment guide](https://example.com/deploy) for steps.";
        let result = render_message_body(md, &theme());
        assert!(result.is_some());
        let text = text_to_string(&result.unwrap());
        assert!(text.contains("deployment guide"), "link text preserved");
        assert!(text.contains("steps"), "surrounding text preserved");
    }

    /// Verify `render_message_body` preserves blockquote content.
    #[test]
    fn c5_message_body_blockquote_fidelity() {
        let md = "> Important: the migration is scheduled for midnight.";
        let result = render_message_body(md, &theme());
        assert!(result.is_some());
        let text = text_to_string(&result.unwrap());
        assert!(text.contains("Important"), "blockquote content preserved");
        assert!(text.contains("midnight"), "blockquote content preserved");
    }

    /// Verify `render_message_body` preserves task list items.
    #[test]
    fn c5_message_body_task_list_fidelity() {
        let md = "- [x] Tests pass\n- [ ] Deploy to staging\n- [ ] Monitor metrics";
        let result = render_message_body(md, &theme());
        assert!(result.is_some());
        let text = text_to_string(&result.unwrap());
        assert!(text.contains("Tests pass"), "checked task preserved");
        assert!(
            text.contains("Deploy to staging"),
            "unchecked task preserved"
        );
        assert!(text.contains("Monitor metrics"), "unchecked task preserved");
    }

    /// Verify `render_message_body_preview` strips markdown but keeps content.
    #[test]
    fn c5_preview_strips_formatting_preserves_content() {
        let md = "## Update\n\n**Build** is `green`. See [CI](https://ci.example.com).";
        let preview = render_message_body_preview(md, 200);
        assert!(preview.is_some());
        let text = preview.unwrap();
        assert!(text.contains("Update"), "heading text preserved in preview");
        assert!(text.contains("Build"), "bold text preserved in preview");
        assert!(text.contains("green"), "code text preserved in preview");
        assert!(
            !text.contains("**"),
            "markdown syntax stripped from preview"
        );
        assert!(!text.contains("##"), "heading syntax stripped from preview");
    }

    /// Verify `render_message_body_blockquote` wraps multi-line content correctly.
    #[test]
    fn c5_blockquote_multiline_fidelity() {
        let body = "Line one\nLine two\nLine three";
        let result = render_message_body_blockquote(body, &theme());
        assert!(result.is_some());
        let text = text_to_string(&result.unwrap());
        assert!(text.contains("Line one"), "first line in blockquote");
        assert!(text.contains("Line three"), "last line in blockquote");
    }

    /// Verify sanitization strips script but preserves surrounding content
    /// through the full `render_message_body` pipeline.
    #[test]
    fn c5_sanitization_preserves_surrounding_content() {
        let md = "Before\n\n<script>evil()</script>\n\nMiddle\n\n<img onerror=x src=y>\n\nAfter";
        let result = render_message_body(md, &theme());
        assert!(result.is_some());
        let text = text_to_string(&result.unwrap());
        assert!(
            text.contains("Before"),
            "content before hostile tag preserved"
        );
        assert!(
            text.contains("Middle"),
            "content between hostile tags preserved"
        );
        assert!(
            text.contains("After"),
            "content after hostile tag preserved"
        );
        assert!(!text.contains("evil"), "script content removed");
        assert!(!text.contains("onerror"), "event handler removed");
    }

    /// Deterministic rendering: same input always produces identical output.
    #[test]
    fn c5_deterministic_rendering_contract() {
        let md = "## Report\n\n- item 1\n- item 2\n\n```\ncode\n```\n\n> quote";
        let t = theme();
        let r1 = render_message_body(md, &t).unwrap();
        let r2 = render_message_body(md, &t).unwrap();
        let s1 = canonical_text(&r1);
        let s2 = canonical_text(&r2);
        assert_eq!(s1, s2, "same input must produce identical output");
    }

    // ── C5: Canonical Pipeline Regression Matrix (br-2k3qx.3.5) ──

    /// Comprehensive regression matrix exercising `render_message_body()` (the
    /// canonical pipeline with JSON detection) across all GFM element types.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn c5_canonical_pipeline_regression_matrix() {
        struct Case {
            id: &'static str,
            body_md: &'static str,
            min_height: usize,
            must_contain: &'static [&'static str],
            must_not_contain: &'static [&'static str],
        }

        let t = theme();
        let cases = [
            Case {
                id: "heading_h1",
                body_md: "# Main Title",
                min_height: 1,
                must_contain: &["Main Title"],
                must_not_contain: &[],
            },
            Case {
                id: "bold_italic_mixed",
                body_md: "Normal **bold** *italic* ***both*** `code`",
                min_height: 1,
                must_contain: &["Normal", "bold", "italic", "both", "code"],
                must_not_contain: &[],
            },
            Case {
                id: "unordered_list",
                body_md: "- apple\n- banana\n- cherry",
                min_height: 3,
                must_contain: &["apple", "banana", "cherry"],
                must_not_contain: &[],
            },
            Case {
                id: "ordered_list",
                body_md: "1. first\n2. second\n3. third",
                min_height: 3,
                must_contain: &["first", "second", "third"],
                must_not_contain: &[],
            },
            Case {
                id: "gfm_table",
                body_md: "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |",
                min_height: 2,
                must_contain: &["Name", "Age", "Alice", "Bob"],
                must_not_contain: &[],
            },
            Case {
                id: "code_fence_rust",
                body_md: "```rust\nfn main() {\n    println!(\"hello\");\n}\n```",
                min_height: 1,
                must_contain: &["fn main", "println"],
                must_not_contain: &[],
            },
            Case {
                id: "link_inline",
                body_md: "See [docs](https://example.com) for info.",
                min_height: 1,
                must_contain: &["See", "docs", "info"],
                must_not_contain: &[],
            },
            Case {
                id: "blockquote",
                body_md: "> Important note\n> Second line",
                min_height: 1,
                must_contain: &["Important note"],
                must_not_contain: &[],
            },
            Case {
                id: "task_list",
                body_md: "- [x] completed\n- [ ] pending",
                min_height: 2,
                must_contain: &["completed", "pending"],
                must_not_contain: &[],
            },
            Case {
                id: "strikethrough",
                body_md: "~~old~~ new",
                min_height: 1,
                must_contain: &["old", "new"],
                must_not_contain: &[],
            },
            Case {
                id: "json_auto_wrapped",
                body_md: "{\"status\":\"ok\",\"count\":42}",
                min_height: 1,
                must_contain: &["status", "ok", "count"],
                must_not_contain: &[],
            },
            Case {
                id: "sanitized_html",
                body_md: "Safe <script>alert('xss')</script> text",
                min_height: 1,
                must_contain: &["Safe", "text"],
                must_not_contain: &["alert", "script"],
            },
            Case {
                id: "mixed_agent_message",
                body_md: "## Build Report\n\n**Status:** passing\n\n- Tests: 42/42\n- Coverage: 95%\n\n```\ncargo test --all\n```\n\n> LGTM — ready to merge",
                min_height: 5,
                must_contain: &[
                    "Build Report",
                    "passing",
                    "42/42",
                    "Coverage",
                    "cargo test",
                    "LGTM",
                ],
                must_not_contain: &[],
            },
        ];

        for case in cases {
            let result = render_message_body(case.body_md, &t);
            assert!(
                result.is_some(),
                "C5 {}: render_message_body returned None for non-empty input",
                case.id
            );
            let text = result.unwrap();
            assert!(
                text.height() >= case.min_height,
                "C5 {}: expected height >= {}, got {}",
                case.id,
                case.min_height,
                text.height()
            );
            let plain = canonical_text(&text);
            for frag in case.must_contain {
                assert!(
                    plain.contains(frag),
                    "C5 {}: missing required fragment {:?} in: {}",
                    case.id,
                    frag,
                    plain
                );
            }
            for frag in case.must_not_contain {
                assert!(
                    !plain.contains(frag),
                    "C5 {}: found prohibited fragment {:?} in: {}",
                    case.id,
                    frag,
                    plain
                );
            }
            // Determinism check
            let r2 = render_message_body(case.body_md, &t).unwrap();
            assert_eq!(
                canonical_text(&text),
                canonical_text(&r2),
                "C5 {}: rendering must be deterministic",
                case.id
            );
        }
    }

    /// Preview regression: canonical previews preserve text content and truncate.
    #[test]
    fn c5_preview_regression_fixtures() {
        let cases = [
            ("**bold** text", "bold text"),
            ("# Heading\n\nParagraph", "Heading Paragraph"),
            ("- item\n- item2", "item item2"),
            ("> quoted text", "quoted text"),
        ];
        for (input, expected_contains) in cases {
            let preview = render_message_body_preview(input, 200);
            assert!(preview.is_some(), "preview should not be None for: {input}");
            let p = preview.unwrap();
            for word in expected_contains.split_whitespace() {
                assert!(
                    p.contains(word),
                    "preview missing {word:?} for input {input:?}, got: {p}"
                );
            }
        }
    }

    /// Blockquote regression: canonical blockquote renders body excerpt correctly.
    #[test]
    fn c5_blockquote_regression_fixtures() {
        let t = theme();
        // Normal text blockquote
        let result = render_message_body_blockquote("Hello from agent", &t);
        assert!(result.is_some());
        let plain = canonical_text(&result.unwrap());
        assert!(plain.contains("Hello from agent"));

        // JSON body in blockquote gets auto-detected
        let result = render_message_body_blockquote("{\"key\":\"value\"}", &t);
        assert!(result.is_some());
        let plain = canonical_text(&result.unwrap());
        assert!(plain.contains("key"));

        // Empty body produces no blockquote
        assert!(render_message_body_blockquote("", &t).is_none());
        assert!(render_message_body_blockquote("   ", &t).is_none());
    }
}
