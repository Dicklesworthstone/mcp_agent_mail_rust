//! Integration test for `am doctor selftest` (pass-6 verb).
//!
//! Invokes `handle_selftest` directly (no subprocess) to verify the
//! 6 chokepoint checks all pass on this machine. Acts as a CI smoke
//! test for the chokepoint primitives that pass-1..6 hardened.
//!
//! If any check fails, this test fails — meaning a regression in
//! mutate/undo/runs would prevent a green CI build.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::handle_selftest;

#[test]
fn doctor_selftest_passes_all_chokepoint_checks() {
    // We capture stdout to verify the JSON envelope shape.
    // Since handle_selftest writes to stdout via println!, we run it
    // and rely on the function's exit-code semantics: returns Ok(()) iff
    // all 6 checks pass.
    let result = handle_selftest(None);
    assert!(
        result.is_ok(),
        "doctor selftest reported failing checks; chokepoint regression?"
    );
}
