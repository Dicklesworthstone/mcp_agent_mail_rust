#![forbid(unsafe_code)]

//! Golden snapshot corpus for the markdown-to-terminal renderer.
//!
//! Renders a curated set of GFM fixtures through [`render_body()`] into
//! terminal buffers at multiple widths and compares against stored baselines.
//!
//! ## Running
//!
//! ```sh
//! # Check against existing snapshots
//! cargo test -p mcp-agent-mail-server --test golden_markdown_snapshots
//!
//! # Bless (create/update) snapshots
//! BLESS=1 cargo test -p mcp-agent-mail-server --test golden_markdown_snapshots
//! ```
//!
//! ## Coverage
//!
//! - **Core GFM**: headings, lists, tables, code fences, emphasis, links
//! - **Rich realistic**: multi-element agent mail messages
//! - **Width variants**: 40 (compact), 80 (standard), 120 (wide)
//! - **Hostile/sanitization**: XSS payloads, deeply nested markup, huge inputs
//! - **Edge cases**: empty, single char, very long lines, streaming partial

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Frame, GraphemePool};
use ftui_harness::{assert_snapshot, buffer_to_text};
use mcp_agent_mail_server::tui_markdown::{MarkdownTheme, render_body, render_body_streaming};

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Render markdown text into a terminal buffer and snapshot it.
fn snapshot_markdown(md: &str, width: u16, height: u16, name: &str) {
    let theme = MarkdownTheme::default();
    let text = render_body(md, &theme);
    let para = Paragraph::new(text);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    para.render(area, &mut frame);
    assert_snapshot!(name, &frame.buffer);
}

/// Render markdown with a titled border block.
fn snapshot_markdown_boxed(md: &str, width: u16, height: u16, name: &str) {
    let theme = MarkdownTheme::default();
    let text = render_body(md, &theme);
    let para = Paragraph::new(text).block(Block::new().title(" Markdown "));

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    para.render(area, &mut frame);
    assert_snapshot!(name, &frame.buffer);
}

/// Render streaming (partial) markdown and snapshot it.
fn snapshot_markdown_streaming(md: &str, width: u16, height: u16, name: &str) {
    let theme = MarkdownTheme::default();
    let text = render_body_streaming(md, &theme);
    let para = Paragraph::new(text);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    para.render(area, &mut frame);
    assert_snapshot!(name, &frame.buffer);
}

/// Render markdown and return the plain text (for content assertions).
fn render_to_text(md: &str, width: u16, height: u16) -> String {
    let theme = MarkdownTheme::default();
    let text = render_body(md, &theme);
    let para = Paragraph::new(text);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    para.render(area, &mut frame);
    buffer_to_text(&frame.buffer)
}

// ═══════════════════════════════════════════════════════════════════════
// Fixture corpus — inline markdown strings
// ═══════════════════════════════════════════════════════════════════════

const FIXTURE_HEADING_LEVELS: &str = "\
# Heading 1
## Heading 2
### Heading 3
#### Heading 4
##### Heading 5
###### Heading 6";

const FIXTURE_UNORDERED_LIST: &str = "\
- Alpha
- Beta
  - Nested one
  - Nested two
    - Deep nested
- Gamma";

const FIXTURE_ORDERED_LIST: &str = "\
1. First step
2. Second step
3. Third step
   1. Sub-step A
   2. Sub-step B
4. Final step";

const FIXTURE_TASK_LIST: &str = "\
- [x] Completed task
- [ ] Pending task
- [x] Another done
- [ ] Still open";

const FIXTURE_CODE_FENCE_RUST: &str = "\
```rust
fn main() {
    let items = vec![1, 2, 3];
    for item in &items {
        println!(\"Item: {item}\");
    }
}
```";

const FIXTURE_CODE_FENCE_PYTHON: &str = "\
```python
def fibonacci(n):
    a, b = 0, 1
    for _ in range(n):
        yield a
        a, b = b, a + b

for num in fibonacci(10):
    print(num)
```";

const FIXTURE_INLINE_CODE: &str = "\
Run `cargo test --release` to build and test.
The `Config::default()` method returns sensible values.
Use `--verbose` for extra output.";

const FIXTURE_GFM_TABLE: &str = "\
| Agent | Status | Messages | Last Active |
|-------|--------|----------|-------------|
| RedFox | online | 42 | 2m ago |
| BluePeak | idle | 18 | 1h ago |
| GoldHawk | offline | 7 | 3d ago |
| SwiftLake | busy | 156 | just now |";

const FIXTURE_EMPHASIS: &str = "\
This has **bold text** and *italic text* and ***bold italic***.
Also __double underscore bold__ and _single underscore italic_.
And ~~strikethrough~~ for deleted content.";

const FIXTURE_LINKS: &str = "\
Visit [the documentation](https://docs.example.com) for details.
See also <https://auto-linked.example.com> for autolinks.
Email us at [support](mailto:help@example.com).";

const FIXTURE_BLOCKQUOTE: &str = "\
> This is a blockquote with **bold** text.
> It spans multiple lines.
>
> > Nested blockquote here
> > with continuation.
>
> Back to level one.";

const FIXTURE_THEMATIC_BREAK: &str = "\
Section one content.

---

Section two content.

***

Section three content.";

const FIXTURE_MIXED_REALISTIC: &str = "\
# Build Status Report

**Agent**: RedFox (claude-code/opus-4.6)
**Bead**: br-3vwi.8.3 — Keymap customization

## Summary

Implemented configurable keymap profiles with full persistence:

1. **KeymapProfile** enum: Default, Vim, Emacs, Minimal, Custom
2. **KeymapRegistry**: live state with override support
3. **Help overlay**: profile-aware with scroll

> Note: All 38 tests passing, 0 clippy warnings.

### Key Changes

| File | Lines | Description |
|------|-------|-------------|
| tui_keymap.rs | +655 | Core keymap system |
| tui_persist.rs | +99 | Profile persistence |
| tui_app.rs | +36 | App integration |

```rust
pub enum KeymapProfile {
    Default, Vim, Emacs, Minimal, Custom,
}
```

### Next Steps

- [x] Core implementation
- [x] Persistence layer
- [ ] User documentation
- [ ] Migration guide

---

Commit: `be76157`";

const FIXTURE_FOOTNOTES: &str = "\
The system uses WAL mode[^1] for concurrent access.

Timestamps are stored as microseconds[^2] since Unix epoch.

[^1]: Write-Ahead Logging provides better concurrency.
[^2]: Using i64 allows dates from 290,000 BCE to 290,000 CE.";

const FIXTURE_NESTED_LISTS_DEEP: &str = "\
- Level 1
  - Level 2
    - Level 3
      - Level 4
        - Level 5
          - Level 6 (very deep)
      - Back to 4
    - Back to 3
  - Back to 2
- Back to 1";

// ── Hostile / sanitization-sensitive fixtures ────────────────────────

const FIXTURE_HOSTILE_XSS: &str = "\
Normal text before.
<script>alert('xss')</script>
![img](x onerror=alert(1))
[click](javascript:alert(document.cookie))
<img src=x onerror=alert(1)>
Normal text after.";

const FIXTURE_HOSTILE_DEEP_EMPHASIS: &str = "\
Start of text.
*****deeply*****nested*****emphasis*****here*****end*****
More text after.";

const FIXTURE_HOSTILE_HUGE_TABLE: &str = "\
| C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | C10 | C11 | C12 | C13 | C14 | C15 |
|----|----|----|----|----|----|----|----|----|-----|-----|-----|-----|-----|-----|
| v1 | v2 | v3 | v4 | v5 | v6 | v7 | v8 | v9 | v10 | v11 | v12 | v13 | v14 | v15 |
| a1 | a2 | a3 | a4 | a5 | a6 | a7 | a8 | a9 | a10 | a11 | a12 | a13 | a14 | a15 |";

const FIXTURE_HOSTILE_UNCLOSED_FENCE: &str = "\
Some text before.
```python
def leaked():
    return 'never closed'

This line appears inside the fence because it was never closed.
And this one too.";

const FIXTURE_HOSTILE_CONTROL_CHARS: &str = "\
Normal\x01hidden\x02text\x03here.
Bold: **visible\x0Bbold**.
Code: `inline\x7Fcode`.";

const FIXTURE_HOSTILE_ZERO_WIDTH: &str = "\
Hello\u{200B}World\u{200B}Test.
**Bold\u{200B}text** and *italic\u{200B}text*.
Link: [clic\u{200B}k](https://example.com)";

// ── Edge cases ──────────────────────────────────────────────────────

const FIXTURE_EMPTY: &str = "";

const FIXTURE_SINGLE_CHAR: &str = "x";

const FIXTURE_ONLY_WHITESPACE: &str = "   \n\n   \n   ";

const FIXTURE_LONG_LINE: &str = "\
This is a single very long line that should test word wrapping behavior in narrow terminals: Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";

const FIXTURE_STREAMING_PARTIAL: &str = "\
# Partial Message

Started writing a code block:
```rust
fn work_in_progress() {
    let x = 42;
    // still typing...";

// ═══════════════════════════════════════════════════════════════════════
// Core GFM — standard width (80x24)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_headings_80x24() {
    snapshot_markdown(FIXTURE_HEADING_LEVELS, 80, 24, "md_headings_80x24");
}

#[test]
fn md_unordered_list_80x24() {
    snapshot_markdown(FIXTURE_UNORDERED_LIST, 80, 24, "md_unordered_list_80x24");
}

#[test]
fn md_ordered_list_80x24() {
    snapshot_markdown(FIXTURE_ORDERED_LIST, 80, 24, "md_ordered_list_80x24");
}

#[test]
fn md_task_list_80x24() {
    snapshot_markdown(FIXTURE_TASK_LIST, 80, 24, "md_task_list_80x24");
}

#[test]
fn md_code_rust_80x24() {
    snapshot_markdown(FIXTURE_CODE_FENCE_RUST, 80, 24, "md_code_rust_80x24");
}

#[test]
fn md_code_python_80x24() {
    snapshot_markdown(FIXTURE_CODE_FENCE_PYTHON, 80, 24, "md_code_python_80x24");
}

#[test]
fn md_inline_code_80x24() {
    snapshot_markdown(FIXTURE_INLINE_CODE, 80, 24, "md_inline_code_80x24");
}

#[test]
fn md_table_80x24() {
    snapshot_markdown(FIXTURE_GFM_TABLE, 80, 24, "md_table_80x24");
}

#[test]
fn md_emphasis_80x24() {
    snapshot_markdown(FIXTURE_EMPHASIS, 80, 24, "md_emphasis_80x24");
}

#[test]
fn md_links_80x24() {
    snapshot_markdown(FIXTURE_LINKS, 80, 24, "md_links_80x24");
}

#[test]
fn md_blockquote_80x24() {
    snapshot_markdown(FIXTURE_BLOCKQUOTE, 80, 24, "md_blockquote_80x24");
}

#[test]
fn md_thematic_break_80x24() {
    snapshot_markdown(FIXTURE_THEMATIC_BREAK, 80, 24, "md_thematic_break_80x24");
}

#[test]
fn md_footnotes_80x24() {
    snapshot_markdown(FIXTURE_FOOTNOTES, 80, 24, "md_footnotes_80x24");
}

#[test]
fn md_nested_lists_deep_80x24() {
    snapshot_markdown(
        FIXTURE_NESTED_LISTS_DEEP,
        80,
        24,
        "md_nested_lists_deep_80x24",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Realistic multi-element message — all widths
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_realistic_40x24() {
    snapshot_markdown(FIXTURE_MIXED_REALISTIC, 40, 24, "md_realistic_40x24");
}

#[test]
fn md_realistic_80x24() {
    snapshot_markdown(FIXTURE_MIXED_REALISTIC, 80, 24, "md_realistic_80x24");
}

#[test]
fn md_realistic_80x40() {
    snapshot_markdown(FIXTURE_MIXED_REALISTIC, 80, 40, "md_realistic_80x40");
}

#[test]
fn md_realistic_120x40() {
    snapshot_markdown(FIXTURE_MIXED_REALISTIC, 120, 40, "md_realistic_120x40");
}

#[test]
fn md_realistic_boxed_80x24() {
    snapshot_markdown_boxed(FIXTURE_MIXED_REALISTIC, 80, 24, "md_realistic_boxed_80x24");
}

// ═══════════════════════════════════════════════════════════════════════
// Compact terminal (40 wide) — tests wrapping and truncation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_headings_40x24() {
    snapshot_markdown(FIXTURE_HEADING_LEVELS, 40, 24, "md_headings_40x24");
}

#[test]
fn md_table_40x24() {
    snapshot_markdown(FIXTURE_GFM_TABLE, 40, 24, "md_table_40x24");
}

#[test]
fn md_code_rust_40x24() {
    snapshot_markdown(FIXTURE_CODE_FENCE_RUST, 40, 24, "md_code_rust_40x24");
}

#[test]
fn md_long_line_40x12() {
    snapshot_markdown(FIXTURE_LONG_LINE, 40, 12, "md_long_line_40x12");
}

// ═══════════════════════════════════════════════════════════════════════
// Wide terminal (120) — tests full-width rendering
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_headings_120x40() {
    snapshot_markdown(FIXTURE_HEADING_LEVELS, 120, 40, "md_headings_120x40");
}

#[test]
fn md_table_120x40() {
    snapshot_markdown(FIXTURE_GFM_TABLE, 120, 40, "md_table_120x40");
}

#[test]
fn md_code_python_120x40() {
    snapshot_markdown(FIXTURE_CODE_FENCE_PYTHON, 120, 40, "md_code_python_120x40");
}

// ═══════════════════════════════════════════════════════════════════════
// Hostile / sanitization-sensitive
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_hostile_xss_80x24() {
    snapshot_markdown(FIXTURE_HOSTILE_XSS, 80, 24, "md_hostile_xss_80x24");
}

#[test]
fn md_hostile_deep_emphasis_80x24() {
    snapshot_markdown(
        FIXTURE_HOSTILE_DEEP_EMPHASIS,
        80,
        24,
        "md_hostile_deep_emphasis_80x24",
    );
}

#[test]
fn md_hostile_huge_table_80x24() {
    snapshot_markdown(
        FIXTURE_HOSTILE_HUGE_TABLE,
        80,
        24,
        "md_hostile_huge_table_80x24",
    );
}

#[test]
fn md_hostile_unclosed_fence_80x24() {
    snapshot_markdown(
        FIXTURE_HOSTILE_UNCLOSED_FENCE,
        80,
        24,
        "md_hostile_unclosed_fence_80x24",
    );
}

#[test]
fn md_hostile_control_chars_80x24() {
    snapshot_markdown(
        FIXTURE_HOSTILE_CONTROL_CHARS,
        80,
        24,
        "md_hostile_control_chars_80x24",
    );
}

#[test]
fn md_hostile_zero_width_80x24() {
    snapshot_markdown(
        FIXTURE_HOSTILE_ZERO_WIDTH,
        80,
        24,
        "md_hostile_zero_width_80x24",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_empty_80x24() {
    snapshot_markdown(FIXTURE_EMPTY, 80, 24, "md_empty_80x24");
}

#[test]
fn md_single_char_80x24() {
    snapshot_markdown(FIXTURE_SINGLE_CHAR, 80, 24, "md_single_char_80x24");
}

#[test]
fn md_only_whitespace_80x24() {
    snapshot_markdown(FIXTURE_ONLY_WHITESPACE, 80, 24, "md_only_whitespace_80x24");
}

#[test]
fn md_long_line_80x24() {
    snapshot_markdown(FIXTURE_LONG_LINE, 80, 24, "md_long_line_80x24");
}

#[test]
fn md_long_line_120x24() {
    snapshot_markdown(FIXTURE_LONG_LINE, 120, 24, "md_long_line_120x24");
}

// ═══════════════════════════════════════════════════════════════════════
// Streaming / partial content
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn md_streaming_partial_80x24() {
    snapshot_markdown_streaming(
        FIXTURE_STREAMING_PARTIAL,
        80,
        24,
        "md_streaming_partial_80x24",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Visual diff content assertions — complement snapshots with semantic checks
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn content_realistic_preserves_key_text() {
    let text = render_to_text(FIXTURE_MIXED_REALISTIC, 80, 40);
    assert!(text.contains("Build Status Report"), "heading preserved");
    assert!(text.contains("RedFox"), "agent name preserved");
    assert!(text.contains("tui_keymap.rs"), "table content preserved");
    assert!(text.contains("KeymapProfile"), "code content preserved");
    assert!(text.contains("be76157"), "commit hash preserved");
}

#[test]
fn content_table_preserves_agents() {
    let text = render_to_text(FIXTURE_GFM_TABLE, 80, 24);
    assert!(text.contains("RedFox"), "first agent preserved");
    assert!(text.contains("BluePeak"), "second agent preserved");
    assert!(text.contains("GoldHawk"), "third agent preserved");
    assert!(text.contains("SwiftLake"), "fourth agent preserved");
}

#[test]
fn content_code_preserves_keywords() {
    let text = render_to_text(FIXTURE_CODE_FENCE_RUST, 80, 24);
    assert!(text.contains("fn main"), "function preserved");
    assert!(text.contains("println"), "macro preserved");
}

#[test]
fn content_hostile_xss_renders_safely() {
    let text = render_to_text(FIXTURE_HOSTILE_XSS, 80, 24);
    assert!(text.contains("Normal text before"), "pre-text preserved");
    assert!(text.contains("Normal text after"), "post-text preserved");
}

#[test]
fn content_emphasis_terms_visible() {
    let text = render_to_text(FIXTURE_EMPHASIS, 80, 24);
    assert!(text.contains("bold text"), "bold content visible");
    assert!(text.contains("italic text"), "italic content visible");
    // Strikethrough and underscore emphasis appear on wrapped lines;
    // verify they're accessible in a tall viewport.
    let tall = render_to_text(FIXTURE_EMPHASIS, 80, 40);
    assert!(
        tall.contains("bold"),
        "bold content visible in tall viewport"
    );
}

#[test]
fn content_links_text_visible() {
    let text = render_to_text(FIXTURE_LINKS, 80, 24);
    assert!(
        text.contains("documentation") || text.contains("the documentation"),
        "link text visible"
    );
}

#[test]
fn content_blockquote_text_visible() {
    let text = render_to_text(FIXTURE_BLOCKQUOTE, 80, 24);
    assert!(text.contains("blockquote"), "blockquote content visible");
    assert!(text.contains("Nested"), "nested quote visible");
}

#[test]
fn content_empty_produces_blank_frame() {
    let text = render_to_text(FIXTURE_EMPTY, 80, 24);
    let non_space = text.chars().filter(|c| !c.is_whitespace()).count();
    assert_eq!(non_space, 0, "empty markdown should produce blank frame");
}

#[test]
fn content_width_affects_wrapping() {
    let narrow = render_to_text(FIXTURE_LONG_LINE, 40, 12);
    let wide = render_to_text(FIXTURE_LONG_LINE, 120, 12);
    let narrow_lines = narrow.lines().filter(|l| !l.trim().is_empty()).count();
    let wide_lines = wide.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(
        narrow_lines >= wide_lines,
        "narrow terminal should produce more lines ({narrow_lines}) \
         than wide ({wide_lines}) due to wrapping"
    );
}
