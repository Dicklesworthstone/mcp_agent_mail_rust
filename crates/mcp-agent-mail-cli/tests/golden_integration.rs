#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn run_am(args: &[&str]) -> Output {
    Command::new(am_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn am")
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("stdout json")
}

#[test]
fn golden_capture_verify_roundtrip_for_single_fixture() {
    let temp = tempdir().expect("tempdir");
    let dir = temp.path().display().to_string();

    let capture = run_am(&[
        "golden",
        "capture",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(
        capture.status.code(),
        Some(0),
        "capture stderr: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let capture_json = stdout_json(&capture);
    assert_eq!(capture_json["total"], 1);
    assert_eq!(capture_json["failed"], 0);

    let verify = run_am(&[
        "golden",
        "verify",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(
        verify.status.code(),
        Some(0),
        "verify stderr: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let verify_json = stdout_json(&verify);
    assert_eq!(verify_json["total"], 1);
    assert_eq!(verify_json["failed"], 0);
    assert_eq!(verify_json["rows"][0]["status"], "ok");
}

#[test]
fn golden_verify_detects_fixture_drift() {
    let temp = tempdir().expect("tempdir");
    let dir = temp.path().display().to_string();

    let capture = run_am(&[
        "golden",
        "capture",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(capture.status.code(), Some(0));

    std::fs::write(temp.path().join("am_help.txt"), "drift\n").expect("mutate fixture");

    let verify = run_am(&[
        "golden",
        "verify",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(verify.status.code(), Some(1), "expected mismatch exit code");
    let verify_json = stdout_json(&verify);
    assert_eq!(verify_json["failed"], 1);
    assert_eq!(verify_json["rows"][0]["status"], "mismatch");
    let diff = verify_json["rows"][0]["diff"]
        .as_str()
        .expect("diff should be present");
    assert!(diff.contains("@@ mismatch around line"));
}

#[test]
fn golden_list_reports_present_stale_and_missing() {
    let temp = tempdir().expect("tempdir");
    let dir = temp.path().display().to_string();

    let capture = run_am(&[
        "golden",
        "capture",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(capture.status.code(), Some(0));

    let list_present = run_am(&[
        "golden",
        "list",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(list_present.status.code(), Some(0));
    assert_eq!(stdout_json(&list_present)["rows"][0]["status"], "present");

    std::fs::write(temp.path().join("am_help.txt"), "drift\n").expect("mutate fixture");
    let list_stale = run_am(&[
        "golden",
        "list",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(list_stale.status.code(), Some(0));
    assert_eq!(stdout_json(&list_stale)["rows"][0]["status"], "stale");

    std::fs::remove_file(temp.path().join("am_help.txt")).expect("remove fixture");
    let list_missing = run_am(&[
        "golden",
        "list",
        "--dir",
        &dir,
        "--filter",
        "am_help.txt",
        "--json",
    ]);
    assert_eq!(list_missing.status.code(), Some(0));
    assert_eq!(stdout_json(&list_missing)["rows"][0]["status"], "missing");
}
