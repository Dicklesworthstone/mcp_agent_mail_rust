//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

mod ci_integration;
mod cli_json_snapshots;
mod corruption_corpus;
mod doctor_capabilities_contract;
mod doctor_cli_smoke;
mod doctor_explain_fallback;
mod doctor_fix_only_integration;
mod doctor_fm_round_trip;
mod doctor_handbook_contract;
mod doctor_mcp_selftest_integration;
mod doctor_property_round_trip;
mod doctor_selftest_integration;
mod doctor_write_selftest_integration;
mod file_reservations_argument_compat;
mod flake_triage_integration;
mod golden_integration;
mod help_snapshots;
mod http_surface_isolation;
mod http_transport_harness;
mod integration_runs;
mod mode_matrix_harness;
mod perf_guardrails;
mod perf_security_regressions;
mod reliability_coverage_ci;
mod robot_golden_snapshots;
mod security_privacy_harness;
mod semantic_conformance;
mod setup_fresh_remote_clients;
mod share_archive_harness;
mod share_verify_decrypt;
mod sibling_discovery_cli;
mod tui_accessibility_harness;
mod tui_transport_harness;

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
