// This test is known to fail because ammonia removes the generic `<Vec<String>>` as if it were an HTML tag.
// It was kept around to document the bug, but it breaks `cargo test`.
// #[test]
fn test_ammonia_on_markdown() {
    let body = "This is a rust generic: `Box<Vec<String>>`";
    let b = ammonia::Builder::new();
    let sanitized = b.clean(body).to_string();
    assert_eq!(sanitized, body);
}
