//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.
#![recursion_limit = "256"]

mod agent_name_parity;
mod auto_name_collision;
mod auto_register_profile;
mod contact_policy_parity;
mod idempotency_tool_acceptance;
mod lifecycle_auth_transport;
mod messaging_error_parity;
mod registration_proof_gate;
mod reservation_error_parity;
mod reservation_regression_fixtures;
mod system_error_parity;
mod tool_input_proptest;
mod transcript_safe_identity;
mod validation_error_parity;

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
