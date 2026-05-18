use mcp_agent_mail_core::pattern_overlap::patterns_overlap;

#[test]
fn test_complex_wildcard_overlap() {
    // Case 1: Intersecting suffix/prefix wildcards
    // Both match "src/ab"
    let p1 = "src/a*";
    let p2 = "src/*b";

    // Fixed logic should detect overlap
    assert!(
        patterns_overlap(p1, p2),
        "Expected overlap detected for src/a* and src/*b"
    );

    // Case 2: Intersecting directory wildcards
    // Both match "src/foo/bar.rs"
    let p3 = "src/foo/*.rs";
    let p4 = "src/*/bar.rs";

    // Fixed logic should detect overlap
    assert!(
        patterns_overlap(p3, p4),
        "Expected overlap detected for src/foo/*.rs and src/*/bar.rs"
    );
}

#[test]
fn test_containment_works() {
    // Case 3: Containment
    let p5 = "src/**/*.rs";
    let p6 = "src/api/*.rs";

    // p5 matches p6 string? Yes.
    assert!(patterns_overlap(p5, p6), "Expected overlap detected");
}

#[test]
fn test_disjoint_does_not_overlap() {
    let p1 = "src/foo/*";
    let p2 = "src/bar/*";
    assert!(!patterns_overlap(p1, p2), "Expected disjoint patterns");
}
