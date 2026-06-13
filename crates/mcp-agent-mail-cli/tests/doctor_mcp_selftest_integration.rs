//! Binary-boundary integration test for `am doctor mcp-selftest`
//! (br-bvq1x.3.3 / C3).
//!
//! Exercises the MCP JSON-RPC decode + dispatch path end-to-end: the parent
//! `am` orchestrates a child `am` that decodes a valid `initialize`, rejects a
//! malformed frame as a PROTOCOL error, negotiates the tool schema, and
//! round-trips a harmless write + read THROUGH the MCP transport against a
//! private scratch mailbox. This guards the ts2 anchor: an MCP decode failure
//! must surface as a protocol problem with version guidance, never a "database
//! error", and a green `health_check` must not imply decode/write health.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn run_mcp_selftest(tempdir: &std::path::Path) -> (i32, String, String) {
    let db_url = format!("sqlite:///{}/storage.sqlite3", tempdir.display());
    let out = Command::new(am_bin())
        .args(["doctor", "mcp-selftest", "--format", "json"])
        .env("AM_INTERFACE_MODE", "cli")
        .env("STORAGE_ROOT", tempdir)
        .env("DATABASE_URL", db_url)
        .env("RUST_LOG", "error")
        .env("LOG_LEVEL", "error")
        .env_remove("HTTP_BEARER_TOKEN")
        .env_remove("AM_DOCTOR_MCP_SELFTEST_INNER")
        .current_dir(tempdir)
        .output()
        .expect("invoke am doctor mcp-selftest");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn mcp_selftest_passes_decode_negotiation_and_round_trip() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, stdout, stderr) = run_mcp_selftest(td.path());

    assert_eq!(
        code, 0,
        "mcp-selftest must exit 0 on a healthy host.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("mcp-selftest must emit valid JSON: {e}\nstdout:\n{stdout}"));

    assert_eq!(envelope["selftest"], "mcp_decode");
    assert_eq!(envelope["ok"], true, "overall verdict must be ok: {envelope}");

    let result = &envelope["result"];
    assert_eq!(result["ok"], true, "inner mcp report must be ok: {result}");
    assert_eq!(result["isolation_verified"], true);
    // A healthy run is not a protocol failure, so no failure class / guidance.
    assert!(
        result["failure_class"].is_null(),
        "healthy run must have no failure_class: {result}"
    );
    assert_eq!(result["expected_protocol_version"], "2024-11-05");
    assert_eq!(
        result["negotiated_protocol_version"], "2024-11-05",
        "server must negotiate the expected protocol version: {result}"
    );

    // Every named check must pass.
    let checks = result["checks"].as_array().expect("checks array");
    let mut seen = std::collections::BTreeSet::new();
    for check in checks {
        assert_eq!(
            check["ok"], true,
            "check `{}` must pass: {check}",
            check["name"]
        );
        if let Some(name) = check["name"].as_str() {
            seen.insert(name.to_string());
        }
    }
    for expected in [
        "decode_round_trip",
        "decode_failure_detected",
        "schema_negotiation",
        "write_transaction",
        "read_after_write",
    ] {
        assert!(
            seen.contains(expected),
            "expected check `{expected}` to be present: {result}"
        );
    }
}
