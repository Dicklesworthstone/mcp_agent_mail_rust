//! Pass-23 integration test: `am doctor explain <fm-id>` falls back
//! to the registry when no recent run has produced a matching finding.
//!
//! Operators frequently call `explain` to learn what an FM does
//! before invoking it. Pre-pass-23, this exited 64 with a confusing
//! "no findings" message because the handler only consulted
//! `.doctor/latest/report.json`. Pass-23 added a registry fallback;
//! these tests pin the behavior.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::CliError;
use mcp_agent_mail_cli::doctor::{fixers, handle_explain};

#[test]
fn explain_registered_fm_id_with_no_doctor_dir_falls_back_to_registry() {
    // Empty tempdir (no .doctor/, no prior runs). explain on a
    // registered FM must succeed via the registry-fallback path.
    let td = tempfile::TempDir::new().expect("tempdir");
    let fm_id = fixers::stale_archive_lock::FM_ID;

    let result = handle_explain(td.path(), fm_id, None);
    assert!(
        result.is_ok(),
        "explain on registered FM with no .doctor/ must succeed via registry fallback, got: {result:?}"
    );
}

#[test]
fn explain_every_registered_fm_id_is_explainable_without_a_run() {
    // Stronger: pass-21 grew the registry to 7 FMs. Every one of
    // them must be explainable from a cold start (no prior runs).
    // Catches the case where a new FM is registered but its id
    // doesn't round-trip through the registry-fallback path.
    let td = tempfile::TempDir::new().expect("tempdir");
    for spec in fixers::registry() {
        let result = handle_explain(td.path(), spec.id, None);
        assert!(
            result.is_ok(),
            "explain on registered FM `{}` failed via fallback: {result:?}",
            spec.id
        );
    }
}

#[test]
fn explain_truly_unknown_id_returns_64() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let result = handle_explain(td.path(), "fm-not-real-pass23-xxx", None);
    let err = result.expect_err("unknown id must error");
    assert!(
        matches!(err, CliError::ExitCode(64)),
        "expected ExitCode(64), got {err:?}"
    );
}
