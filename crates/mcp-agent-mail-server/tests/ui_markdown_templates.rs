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
