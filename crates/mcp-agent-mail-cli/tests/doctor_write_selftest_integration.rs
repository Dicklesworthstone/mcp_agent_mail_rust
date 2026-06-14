//! Binary-boundary integration test for `am doctor write-selftest`
//! (br-bvq1x.3.2 / C2).
//!
//! This exercises the *real* write path end-to-end: the parent `am`
//! orchestrates a child `am` that runs `ensure_project → register_agent ×2 →
//! send_message → acknowledge_message → file_reservation_paths →
//! release_file_reservations → list_agents` against a private scratch mailbox,
//! then reports a per-dimension verdict. It proves write-path liveness (which a
//! read-derived `health_check` cannot) and that the scratch mailbox is
//! genuinely isolated from the operator's archive.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

/// Run `am doctor write-selftest` with CLI mode and the *parent's* storage
/// isolated to `tempdir`. The orchestrator allocates its own private scratch
/// dir for the actual selftest, so the parent's `STORAGE_ROOT` here only keeps
/// startup happy — the real run never touches it.
fn run_write_selftest(tempdir: &std::path::Path) -> (i32, String, String) {
    let db_url = format!("sqlite:///{}/storage.sqlite3", tempdir.display());
    let out = Command::new(am_bin())
        .args(["doctor", "write-selftest", "--format", "json"])
        .env("AM_INTERFACE_MODE", "cli")
        .env("STORAGE_ROOT", tempdir)
        .env("DATABASE_URL", db_url)
        .env("RUST_LOG", "error")
        .env("LOG_LEVEL", "error")
        .env_remove("HTTP_BEARER_TOKEN")
        // Ensure the sentinel is never inherited from the caller's env, so the
        // parent runs the orchestrator path (not the inner path).
        .env_remove("AM_DOCTOR_WRITE_SELFTEST_INNER")
        .current_dir(tempdir)
        .output()
        .expect("invoke am doctor write-selftest");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn write_selftest_passes_all_dimensions_in_isolated_scratch() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, stdout, stderr) = run_write_selftest(td.path());

    assert_eq!(
        code, 0,
        "write-selftest must exit 0 on a healthy host.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("write-selftest must emit valid JSON: {e}\nstdout:\n{stdout}"));

    assert_eq!(envelope["selftest"], "write_path");
    assert_eq!(
        envelope["ok"], true,
        "overall verdict must be ok: {envelope}"
    );

    let result = &envelope["result"];
    assert_eq!(result["ok"], true, "inner sequence must be ok: {result}");
    assert_eq!(
        result["isolation_verified"], true,
        "isolation guard must have confirmed the scratch root"
    );
    assert!(
        result["failing_dimension"].is_null(),
        "no dimension should fail on a healthy host: {result}"
    );

    for dim in ["transport", "schema", "lock", "corruption", "permissions"] {
        assert_eq!(
            result["dimensions"][dim]["status"], "pass",
            "dimension `{dim}` must pass: {result}"
        );
    }

    // Every step in the canonical sequence must have run and passed.
    let steps = result["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 8, "all 8 steps must be present: {result}");
    for step in steps {
        assert_eq!(
            step["ok"], true,
            "step `{}` must pass: {step}",
            step["name"]
        );
        assert_eq!(step["skipped"], false, "no step should be skipped: {step}");
    }

    // The scratch storage root must be a private temp dir, never the parent's.
    let scratch_root = envelope["scratch_storage_root"]
        .as_str()
        .expect("scratch_storage_root string");
    assert!(
        !scratch_root.starts_with(&td.path().to_string_lossy().to_string()),
        "scratch root {scratch_root} must be isolated from the parent STORAGE_ROOT"
    );
}
