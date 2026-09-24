//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

mod fault_injection;
mod parser_filter_fusion_rerank;
mod query_assistance_explain;

#[test]
fn every_test_file_is_linked() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let missing =
        mcp_agent_mail_test_helpers::unlinked_test_files(&tests_dir, "it", include_str!("it.rs"));
    assert!(
        missing.is_empty(),
        "tests/*.rs not declared in tests/it.rs (add `mod <name>;`): {missing:?}"
    );
}
