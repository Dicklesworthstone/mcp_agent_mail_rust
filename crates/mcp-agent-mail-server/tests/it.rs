//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

/// Shared helpers (tests/common/mod.rs); use them as `crate::common`.
mod common;

mod alien_integration;
mod atc_notification_admission;
mod console_output;
mod e2e_atc_learning_loop;
mod fixture_matrix;
mod golden_markdown_snapshots;
mod golden_snapshots;
mod health_endpoints;
mod health_sweep_orphan_refs;
mod http_logging;
mod observability_schemas;
mod pty_e2e_search;
mod segfault_toast;
mod startup_compat;
mod truthfulness_integration;
mod tui_perf_baselines;
mod tui_soak_replay;
mod ui_markdown_templates;
mod web_ui_parity_contract_guard;
mod workers;

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
