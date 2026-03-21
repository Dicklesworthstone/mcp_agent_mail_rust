#[test]
fn test_ammonia_javascript_href() {
    let raw = "<a href=\"javascript:alert(1)\">test</a>";
    let cleaned = crate::markdown::render_markdown_to_safe_html(raw);
    assert!(!cleaned.contains("javascript:"), "javascript should be stripped, got: {}", cleaned);
}
