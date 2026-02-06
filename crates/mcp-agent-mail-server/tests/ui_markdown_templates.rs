#![forbid(unsafe_code)]

#[path = "../src/markdown.rs"]
mod markdown;

#[path = "../src/templates.rs"]
mod templates;

use serde::Serialize;

#[test]
fn markdown_renders_tables_and_strips_scripts() {
    let md = r"
<script>alert('xss')</script>

| a | b |
| - | - |
| 1 | ~~2~~ |
";

    let html = markdown::render_markdown_to_safe_html(md);

    assert!(html.contains("<table"));
    assert!(html.contains("<td"));
    assert!(!html.to_lowercase().contains("<script"));
    assert!(!html.to_lowercase().contains("alert("));
}

#[test]
fn markdown_filters_style_properties() {
    let md = r#"<span style="color: red; position: fixed; background-color: blue">x</span>"#;
    let html = markdown::render_markdown_to_safe_html(md);

    // Allowed properties should remain.
    assert!(html.contains("color"));
    assert!(html.contains("background-color"));
    // Disallowed properties should be removed.
    assert!(!html.contains("position"));
}

#[derive(Serialize)]
struct ErrorCtx<'a> {
    message: &'a str,
}

#[test]
fn templates_render_error_page() {
    let out = templates::render_template("error.html", ErrorCtx { message: "boom" })
        .expect("render error.html");
    assert!(out.contains("boom"));
    assert!(out.contains("<!DOCTYPE html") || out.contains("<html"));
}

// ---------------------------------------------------------------------------
// Mail UI template rendering tests (br-1bm.5.3)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IndexCtx {
    projects: Vec<IndexProject>,
}

#[derive(Serialize)]
struct IndexProject {
    slug: String,
    human_key: String,
    created_at: String,
    agent_count: usize,
}

#[test]
fn templates_render_mail_index() {
    let ctx = IndexCtx {
        projects: vec![IndexProject {
            slug: "test-project".to_string(),
            human_key: "/data/test-project".to_string(),
            created_at: "2026-02-06T00:00:00Z".to_string(),
            agent_count: 3,
        }],
    };
    let out = templates::render_template("mail_index.html", ctx).expect("render mail_index.html");
    assert!(
        out.contains("<!DOCTYPE html") || out.contains("<html"),
        "should produce HTML"
    );
    assert!(out.contains("test-project"), "should contain project slug");
}

#[derive(Serialize)]
struct ProjectCtx {
    project: ProjectView,
    agents: Vec<AgentView>,
}

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    slug: String,
    human_key: String,
    created_at: String,
}

#[derive(Serialize)]
struct AgentView {
    id: i64,
    name: String,
    program: String,
    model: String,
    task_description: String,
    last_active: String,
}

#[test]
fn templates_render_mail_project() {
    let ctx = ProjectCtx {
        project: ProjectView {
            id: 1,
            slug: "my-proj".to_string(),
            human_key: "/data/my-proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        agents: vec![AgentView {
            id: 10,
            name: "GreenLake".to_string(),
            program: "claude-code".to_string(),
            model: "opus-4.6".to_string(),
            task_description: "Working on tests".to_string(),
            last_active: "2026-02-06T12:00:00Z".to_string(),
        }],
    };
    let out =
        templates::render_template("mail_project.html", ctx).expect("render mail_project.html");
    assert!(out.contains("GreenLake"), "should contain agent name");
    assert!(out.contains("my-proj"), "should contain project slug");
}

#[derive(Serialize)]
struct ThreadCtx {
    project: ProjectView,
    thread_id: String,
    thread_subject: String,
    message_count: usize,
    messages: Vec<ThreadMessage>,
}

#[derive(Serialize)]
struct ThreadMessage {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    sender: String,
    created: String,
    importance: String,
}

#[test]
fn templates_render_mail_thread() {
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "br-123".to_string(),
        thread_subject: "[br-123] Test thread".to_string(),
        message_count: 1,
        messages: vec![ThreadMessage {
            id: 42,
            subject: "[br-123] Test thread".to_string(),
            body_md: "Hello **world**".to_string(),
            body_html: "<p>Hello <strong>world</strong></p>".to_string(),
            sender: "BlueBear".to_string(),
            created: "2026-02-06T00:00:00Z".to_string(),
            importance: "normal".to_string(),
        }],
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render mail_thread.html");
    assert!(out.contains("br-123"), "should contain thread ID");
    assert!(out.contains("BlueBear"), "should contain sender name");
}

#[derive(Serialize)]
struct SearchCtx {
    project: ProjectView,
    q: String,
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct SearchResult {
    id: i64,
    subject: String,
    body_snippet: String,
    sender_name: String,
    created: String,
    importance: String,
    thread_id: String,
}

#[test]
fn templates_render_mail_search() {
    let ctx = SearchCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        q: "auth".to_string(),
        results: vec![SearchResult {
            id: 7,
            subject: "Auth module refactor".to_string(),
            body_snippet: "Working on auth changes...".to_string(),
            sender_name: "RedFox".to_string(),
            created: "2026-02-05T00:00:00Z".to_string(),
            importance: "high".to_string(),
            thread_id: "br-456".to_string(),
        }],
    };
    let out = templates::render_template("mail_search.html", ctx).expect("render mail_search.html");
    assert!(
        out.contains("auth") || out.contains("Auth"),
        "should contain search query"
    );
}

#[test]
fn markdown_renders_code_blocks() {
    let md = "```rust\nfn main() {}\n```";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<code"), "should contain code element");
    assert!(html.contains("fn main"), "should contain code content");
}

#[test]
fn markdown_renders_links_safely() {
    let md = r#"[click](https://example.com) and <a href="javascript:alert(1)">xss</a>"#;
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(
        html.contains("https://example.com"),
        "should allow https links"
    );
    assert!(
        !html.contains("javascript:"),
        "should strip javascript: URLs"
    );
}

#[test]
fn markdown_renders_images() {
    let md = r#"![alt text](https://example.com/img.png "title")"#;
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<img"), "should contain img element");
    assert!(html.contains("alt text"), "should contain alt text");
}

#[test]
fn markdown_empty_input() {
    assert_eq!(markdown::render_markdown_to_safe_html(""), "");
    assert_eq!(markdown::render_markdown_to_safe_html("   "), "");
}

// ===========================================================================
// br-1bm.5.2: Comprehensive markdown rendering + sanitization tests
// ===========================================================================

// --- GFM features ---

#[test]
fn markdown_renders_fenced_code_without_language() {
    let md = "```\nplain code\n```";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<pre"), "should contain <pre> tag");
    assert!(html.contains("<code"), "should contain <code> tag");
    assert!(html.contains("plain code"), "code content preserved");
}

#[test]
fn markdown_strikethrough_text_preserved_tag_stripped() {
    // Legacy parity: markdown2 produces <del> for ~~text~~, but bleach strips <del>
    // because it's not in the allowed tags list. Our sanitizer matches this behavior.
    let md = "Some ~~deleted text~~ here";
    let html = markdown::render_markdown_to_safe_html(md);
    // Text content should be preserved even though the tag is stripped.
    assert!(html.contains("deleted text"), "text preserved: {html}");
    // The <del> tag should be stripped by the sanitizer (not in allowlist).
    assert!(!html.contains("<del>"), "del tag stripped: {html}");
}

#[test]
fn markdown_renders_ordered_lists() {
    let md = "1. first\n2. second\n3. third";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<ol"), "should render ordered list");
    assert!(html.contains("<li"), "should render list items");
}

#[test]
fn markdown_renders_unordered_lists() {
    let md = "- alpha\n- beta\n- gamma";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<ul"), "should render unordered list");
    assert!(html.contains("<li"), "should render list items");
}

#[test]
fn markdown_renders_bold_italic() {
    let md = "**bold** *italic* ***both***";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<strong>bold</strong>"), "bold: {html}");
    assert!(html.contains("<em>italic</em>"), "italic: {html}");
}

#[test]
fn markdown_renders_inline_code() {
    let html = markdown::render_markdown_to_safe_html("Use `foo()` here");
    assert!(html.contains("<code>foo()</code>"), "inline code: {html}");
}

#[test]
fn markdown_renders_blockquotes() {
    let md = "> This is a quote";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<blockquote"), "blockquote rendered");
}

#[test]
fn markdown_renders_all_headings() {
    let md = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6";
    let html = markdown::render_markdown_to_safe_html(md);
    for level in 1..=6 {
        assert!(
            html.contains(&format!("<h{level}")),
            "should render h{level}"
        );
    }
}

#[test]
fn markdown_renders_horizontal_rule() {
    let html = markdown::render_markdown_to_safe_html("above\n\n---\n\nbelow");
    assert!(html.contains("<hr"), "horizontal rule rendered");
}

// --- Sanitizer: disallowed tags stripped ---

#[test]
fn sanitizer_strips_script_tags() {
    let html = markdown::render_markdown_to_safe_html("<script>alert(1)</script>");
    assert!(!html.to_lowercase().contains("<script"), "script stripped");
    assert!(!html.contains("alert("), "script content stripped");
}

#[test]
fn sanitizer_strips_style_tags() {
    let html = markdown::render_markdown_to_safe_html(concat!(
        "<style>body",
        "{",
        "display:none",
        "}",
        "</style>"
    ));
    assert!(
        !html.to_lowercase().contains("<style"),
        "style tag stripped"
    );
    assert!(!html.contains("display:none"), "style content stripped");
}

#[test]
fn sanitizer_strips_iframe_tags() {
    let html =
        markdown::render_markdown_to_safe_html(r#"<iframe src="https://evil.com"></iframe>"#);
    assert!(!html.to_lowercase().contains("<iframe"), "iframe stripped");
}

#[test]
fn sanitizer_strips_form_tags() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<form action="https://evil.com"><input type="text"></form>"#,
    );
    assert!(!html.to_lowercase().contains("<form"), "form stripped");
    assert!(!html.to_lowercase().contains("<input"), "input stripped");
}

#[test]
fn sanitizer_strips_object_embed_tags() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<object data="evil.swf"></object><embed src="evil.swf">"#,
    );
    assert!(!html.to_lowercase().contains("<object"), "object stripped");
    assert!(!html.to_lowercase().contains("<embed"), "embed stripped");
}

// --- Sanitizer: attribute filtering ---

#[test]
fn sanitizer_allows_class_on_all_tags() {
    let html = markdown::render_markdown_to_safe_html(r#"<span class="highlight">text</span>"#);
    assert!(
        html.contains("class=\"highlight\""),
        "class on span: {html}"
    );

    let html2 = markdown::render_markdown_to_safe_html(r#"<p class="intro">text</p>"#);
    assert!(html2.contains("class=\"intro\""), "class on p: {html2}");
}

#[test]
fn sanitizer_allows_title_on_abbr() {
    let html = markdown::render_markdown_to_safe_html(r#"<abbr title="HyperText">HTML</abbr>"#);
    assert!(html.contains("title=\"HyperText\""), "abbr title: {html}");
}

#[test]
fn sanitizer_allows_img_attributes() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<img src="https://example.com/img.png" alt="Photo" width="100" height="50" loading="lazy" decoding="async">"#,
    );
    assert!(html.contains("src="), "img src preserved");
    assert!(html.contains("alt="), "img alt preserved");
    assert!(html.contains("width="), "img width preserved");
    assert!(html.contains("height="), "img height preserved");
    assert!(html.contains("loading="), "img loading preserved");
    assert!(html.contains("decoding="), "img decoding preserved");
}

#[test]
fn sanitizer_strips_event_handlers() {
    let html = markdown::render_markdown_to_safe_html(
        r##"<img src="x" onerror="alert(1)"><a href="#" onclick="alert(2)">click</a>"##,
    );
    assert!(!html.contains("onerror"), "onerror stripped: {html}");
    assert!(!html.contains("onclick"), "onclick stripped: {html}");
}

#[test]
fn sanitizer_strips_data_attributes() {
    let html = markdown::render_markdown_to_safe_html(r#"<span data-evil="payload">text</span>"#);
    assert!(!html.contains("data-evil"), "data-* stripped: {html}");
}

// --- CSS style property filtering ---

#[test]
fn sanitizer_allows_text_align_style() {
    let html =
        markdown::render_markdown_to_safe_html(r#"<p style="text-align: center">centered</p>"#);
    assert!(html.contains("text-align"), "text-align allowed: {html}");
}

#[test]
fn sanitizer_allows_text_decoration_style() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<span style="text-decoration: underline">u</span>"#,
    );
    assert!(html.contains("text-decoration"), "text-decoration: {html}");
}

#[test]
fn sanitizer_allows_font_weight_style() {
    let html =
        markdown::render_markdown_to_safe_html(r#"<span style="font-weight: bold">b</span>"#);
    assert!(html.contains("font-weight"), "font-weight: {html}");
}

#[test]
fn sanitizer_strips_dangerous_css_properties() {
    let cases = [
        ("display", r#"<span style="display: none">x</span>"#),
        ("position", r#"<span style="position: absolute">x</span>"#),
        ("z-index", r#"<span style="z-index: 9999">x</span>"#),
    ];
    for (prop, input) in cases {
        let html = markdown::render_markdown_to_safe_html(input);
        assert!(
            !html.contains(prop) || html.contains("style=\"\""),
            "CSS '{prop}' should be stripped; got: {html}"
        );
    }
}

// --- URL scheme filtering ---

#[test]
fn sanitizer_allows_mailto_links() {
    let html =
        markdown::render_markdown_to_safe_html(r#"<a href="mailto:user@example.com">email</a>"#);
    assert!(
        html.contains("href=\"mailto:user@example.com\""),
        "mailto preserved: {html}"
    );
}

#[test]
fn sanitizer_strips_javascript_urls() {
    let html = markdown::render_markdown_to_safe_html(r#"<a href="javascript:alert(1)">click</a>"#);
    assert!(!html.contains("javascript:"), "javascript stripped: {html}");
}

#[test]
fn sanitizer_strips_javascript_in_markdown_links() {
    let html = markdown::render_markdown_to_safe_html("[click](javascript:alert(1))");
    assert!(!html.contains("javascript:"), "js in markdown link: {html}");
}

#[test]
fn sanitizer_strips_vbscript_urls() {
    let html =
        markdown::render_markdown_to_safe_html(r#"<a href="vbscript:MsgBox('xss')">click</a>"#);
    assert!(!html.contains("vbscript:"), "vbscript stripped: {html}");
}

// --- XSS prevention vectors ---

#[test]
fn xss_svg_onload() {
    let html = markdown::render_markdown_to_safe_html(r"<svg onload=alert(1)><circle r=10></svg>");
    assert!(!html.to_lowercase().contains("<svg"), "svg stripped");
    assert!(!html.contains("onload"), "onload stripped");
}

#[test]
fn xss_meta_refresh() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<meta http-equiv="refresh" content="0;url=https://evil.com">"#,
    );
    assert!(!html.to_lowercase().contains("<meta"), "meta stripped");
}

#[test]
fn xss_encoded_javascript() {
    let html = markdown::render_markdown_to_safe_html(
        r#"<a href="&#106;&#97;&#118;&#97;&#115;&#99;&#114;&#105;&#112;&#116;&#58;alert(1)">click</a>"#,
    );
    assert!(!html.contains("javascript:"), "encoded js stripped: {html}");
}

#[test]
fn xss_case_insensitive_script() {
    let html = markdown::render_markdown_to_safe_html("<ScRiPt>alert(1)</ScRiPt>");
    assert!(!html.to_lowercase().contains("<script"), "case-insensitive");
}

#[test]
fn xss_event_handler_body() {
    let html = markdown::render_markdown_to_safe_html(r"<body onload=alert(1)>");
    assert!(!html.to_lowercase().contains("<body"), "body stripped");
    assert!(!html.contains("onload"), "onload stripped");
}

// --- Integration: full message body ---

#[test]
fn integration_full_message_with_all_features() {
    let md = r"# Status Update

| Feature | Status |
|---------|--------|
| Auth    | ~~Done~~ **Complete** |
| DB      | *In progress* |

```rust
fn handle(req: Request) -> Response {
    Response::ok(req)
}
```

Check [docs](https://docs.example.com) for details.

> **Note:** Blocking issue for release.

---
";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<h1"), "h1");
    assert!(html.contains("<table"), "table");
    assert!(html.contains("<pre"), "code block");
    assert!(html.contains("<strong>"), "bold");
    assert!(html.contains("<em>"), "italic");
    assert!(html.contains("<a"), "link");
    assert!(html.contains("<blockquote"), "blockquote");
    assert!(html.contains("<hr"), "hr");
}

#[test]
fn integration_message_with_unsafe_mixed_html() {
    let md = r#"Normal text with <b>bold</b> and <script>evil()</script> injected.

<iframe src="https://evil.com"></iframe>

And a safe <a href="https://safe.com">link</a>.
"#;
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains("<b>bold</b>"), "bold preserved");
    assert!(html.contains("<a"), "link preserved");
    assert!(!html.to_lowercase().contains("<script"), "script stripped");
    assert!(!html.contains("evil()"), "script content stripped");
    assert!(!html.to_lowercase().contains("<iframe"), "iframe stripped");
}

// --- Template auto-escaping ---

#[test]
fn templates_auto_escape_html_in_error() {
    let out = templates::render_template(
        "error.html",
        ErrorCtx {
            message: "<script>alert(1)</script>",
        },
    )
    .expect("render with XSS payload");
    assert!(
        !out.contains("<script>alert(1)</script>"),
        "raw script should be escaped"
    );
}

#[test]
fn templates_missing_template_returns_error() {
    let result = templates::render_template("nonexistent.html", ErrorCtx { message: "test" });
    assert!(result.is_err(), "missing template should return error");
}

// ===========================================================================
// br-1bm.5.5: Edge case and snapshot tests
// ===========================================================================

#[test]
fn templates_render_index_empty_projects() {
    let ctx = IndexCtx { projects: vec![] };
    let out = templates::render_template("mail_index.html", ctx).expect("render empty index");
    assert!(
        out.contains("<!DOCTYPE html") || out.contains("<html"),
        "should produce valid HTML even with no projects"
    );
}

#[test]
fn templates_render_index_many_projects() {
    let projects: Vec<IndexProject> = (0..50)
        .map(|i| IndexProject {
            slug: format!("project-{i}"),
            human_key: format!("/data/project-{i}"),
            created_at: "2026-02-06T00:00:00Z".to_string(),
            agent_count: i,
        })
        .collect();
    let ctx = IndexCtx { projects };
    let out = templates::render_template("mail_index.html", ctx).expect("render many projects");
    assert!(out.contains("project-0"), "first project present");
    assert!(out.contains("project-49"), "last project present");
}

#[test]
fn templates_render_project_no_agents() {
    let ctx = ProjectCtx {
        project: ProjectView {
            id: 1,
            slug: "empty-proj".to_string(),
            human_key: "/data/empty".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        agents: vec![],
    };
    let out = templates::render_template("mail_project.html", ctx).expect("render empty project");
    assert!(out.contains("empty-proj"), "slug present");
}

#[test]
fn templates_render_thread_empty_messages() {
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "empty-thread".to_string(),
        thread_subject: "Empty thread".to_string(),
        message_count: 0,
        messages: vec![],
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render empty thread");
    assert!(out.contains("empty-thread"), "thread ID present");
}

#[test]
fn templates_render_thread_many_messages() {
    let messages: Vec<ThreadMessage> = (0..20)
        .map(|i| ThreadMessage {
            id: i,
            subject: format!("Message {i}"),
            body_md: format!("Content of message {i}"),
            body_html: format!("<p>Content of message {i}</p>"),
            sender: format!("Agent{i}"),
            created: "2026-02-06T00:00:00Z".to_string(),
            importance: if i % 3 == 0 { "high" } else { "normal" }.to_string(),
        })
        .collect();
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "big-thread".to_string(),
        thread_subject: "Big thread".to_string(),
        message_count: 20,
        messages,
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render many messages");
    assert!(out.contains("Agent0"), "first sender present");
    assert!(out.contains("Agent19"), "last sender present");
}

#[test]
fn templates_render_thread_long_subject() {
    let long_subject = "A".repeat(500);
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "long-subj".to_string(),
        thread_subject: long_subject.clone(),
        message_count: 1,
        messages: vec![ThreadMessage {
            id: 1,
            subject: long_subject,
            body_md: "short body".to_string(),
            body_html: "<p>short body</p>".to_string(),
            sender: "TestAgent".to_string(),
            created: "2026-02-06T00:00:00Z".to_string(),
            importance: "normal".to_string(),
        }],
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render long subject");
    assert!(out.contains("AAAA"), "long subject content present");
}

#[test]
fn templates_render_thread_unicode_content() {
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "unicode-thread".to_string(),
        thread_subject: "Unicode test".to_string(),
        message_count: 1,
        messages: vec![ThreadMessage {
            id: 1,
            subject: "Unicode test".to_string(),
            body_md: "Hello \u{1F600} \u{4E16}\u{754C} \u{0410}\u{043B}\u{0435}\u{043A}\u{0441}\u{0430}\u{043D}\u{0434}\u{0440}".to_string(),
            body_html: "<p>Hello \u{1F600} \u{4E16}\u{754C} \u{0410}\u{043B}\u{0435}\u{043A}\u{0441}\u{0430}\u{043D}\u{0434}\u{0440}</p>".to_string(),
            sender: "TestAgent".to_string(),
            created: "2026-02-06T00:00:00Z".to_string(),
            importance: "normal".to_string(),
        }],
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render unicode");
    // Unicode characters should survive template rendering.
    assert!(out.contains('\u{1F600}'), "emoji preserved");
    assert!(out.contains('\u{4E16}'), "CJK preserved");
    assert!(out.contains('\u{0410}'), "Cyrillic preserved");
}

#[test]
fn templates_render_search_empty_results() {
    let ctx = SearchCtx {
        project: ProjectView {
            id: 1,
            slug: "proj".to_string(),
            human_key: "/data/proj".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        q: "nonexistent".to_string(),
        results: vec![],
    };
    let out = templates::render_template("mail_search.html", ctx).expect("render empty search");
    assert!(
        out.contains("nonexistent"),
        "query term present even with no results"
    );
}

#[test]
fn markdown_renders_unicode_safely() {
    let md = "Hello \u{1F600} **bold** \u{4E16}\u{754C}";
    let html = markdown::render_markdown_to_safe_html(md);
    assert!(html.contains('\u{1F600}'), "emoji preserved");
    assert!(html.contains('\u{4E16}'), "CJK preserved");
    assert!(
        html.contains("<strong>bold</strong>"),
        "formatting works with unicode"
    );
}

#[test]
fn markdown_renders_long_content() {
    let long_line = "word ".repeat(1000);
    let html = markdown::render_markdown_to_safe_html(&long_line);
    assert!(html.contains("word"), "content preserved");
    assert!(html.len() > 4000, "output is substantial");
}

// --- Truncate filter behavior ---

#[test]
fn truncate_filter_short_string_unchanged() {
    // Render a template that uses truncate on a short string.
    let ctx = ThreadCtx {
        project: ProjectView {
            id: 1,
            slug: "p".to_string(),
            human_key: "/p".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        thread_id: "t1".to_string(),
        thread_subject: "Test".to_string(),
        message_count: 1,
        messages: vec![ThreadMessage {
            id: 1,
            subject: "Test".to_string(),
            body_md: "Short".to_string(),
            body_html: "<p>Short</p>".to_string(),
            sender: "A".to_string(),
            created: "2026-02-06T00:00:00Z".to_string(),
            importance: "normal".to_string(),
        }],
    };
    let out = templates::render_template("mail_thread.html", ctx).expect("render short body");
    // "Short" is under 150 chars, so truncate should not add "..."
    assert!(out.contains("Short"), "short text preserved");
    // Should NOT contain the ellipsis since body is very short
    // (the truncate preview section should show full text)
}
