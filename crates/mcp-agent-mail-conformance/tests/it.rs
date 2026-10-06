//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

mod conformance;
mod conformance_debug;
mod contact_enforcement_outage;
mod doc_consistency;
mod error_code_parity;
mod protocol_compliance;
mod resource_coverage_guard;
mod resource_description_parity;
mod tool_description_parity;

/// The one lock every module's env/fixture lock returns (br-odkc4).
///
/// All modules of this binary mutate the process environment
/// (`DATABASE_URL`, `STORAGE_ROOT`, ...) and share the process-global
/// write-back queue and its sticky durability flag. Per-module locks let a
/// test in one module run while another module's test had swapped those out.
pub(crate) fn process_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Drain the process-global write-back queue and async commit coalescer.
/// Env guards call this on drop, before their test's tempdir is removed, so
/// queued archive writes never outlive the directory they target.
pub(crate) fn drain_shared_storage() {
    mcp_agent_mail_storage::wbq_flush();
    mcp_agent_mail_storage::flush_async_commits();
}

/// Settle what an earlier test left queued and clear the sticky durability
/// flag, so any degradation (or stalled commit drain that `health_check`
/// reports) a test sees is its own. Call only while holding
/// [`process_env_lock`].
pub(crate) fn settle_shared_storage() {
    drain_shared_storage();
    mcp_agent_mail_storage::clear_durability_degraded();
}

// The workspace-level release/docs drift guard. It includes doc_consistency.rs
// and resource_coverage_guard.rs again under its own module, as it did when it
// was a separate test binary, so the one `docs_drift_ci::` gate filter selects
// the whole docs-drift suite; the duplicate inclusion is deliberate.
#[allow(clippy::duplicate_mod)]
#[path = "../../../tests/docs_drift_ci.rs"]
mod docs_drift_ci;

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
