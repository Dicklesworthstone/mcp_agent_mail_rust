//! Pass-29 binary-boundary smoke tests for the doctor CLI.
//!
//! All prior doctor tests drive `dispatch_only` / `handle_*` directly
//! through the library API. That misses regressions at the binary
//! boundary: clap parsing, exit-code mapping, stdout vs stderr
//! separation, and CLI-mode dual-interface gating. These tests invoke
//! `am` via `std::process::Command` and verify the JSON envelopes
//! agents actually see.
//!
//! Tests are hermetic: each sets `STORAGE_ROOT` and `DATABASE_URL`
//! to a tempdir so production state is never touched.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

/// Run `am <args>` with CLI mode forced + storage isolated to
/// `tempdir`. Returns (exit_code, stdout, stderr). Inherits the
/// caller's PATH so `am` can find `git`, but overrides every env
/// var the doctor consults to keep production state untouched.
fn run_am(tempdir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let bin = am_bin();
    let db_url = format!("sqlite:///{}/storage.sqlite3", tempdir.display());
    let out = Command::new(bin)
        .args(args)
        .env("AM_INTERFACE_MODE", "cli")
        .env("STORAGE_ROOT", tempdir)
        .env("DATABASE_URL", db_url)
        .env("AM_DOCTOR_BACKUPS_DIR", tempdir.join(".doctor"))
        // Don't let the test inherit the operator's HTTP_BEARER_TOKEN
        // etc. — the doctor's wrong-mcp-url FM compares against the
        // canonical URL derived from HTTP_HOST/PORT/PATH.
        .env_remove("HTTP_BEARER_TOKEN")
        .env_remove("AM_DOCTOR_YES")
        .current_dir(tempdir)
        .output()
        .expect("invoke am");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn am_doctor_fixers_emits_registry_as_json() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, stdout, stderr) = run_am(td.path(), &["doctor", "fixers", "--format", "json"]);
    assert_eq!(code, 0, "am doctor fixers must exit 0 (stderr: {stderr})");
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("am doctor fixers must emit valid JSON");

    // Pass-14 contract: envelope has schema_version + tool + fixers[].
    assert_eq!(v["schema_version"], "1.0");
    assert_eq!(v["tool"], "am");
    let fixers = v["fixers"].as_array().expect("fixers must be an array");
    assert!(
        fixers.len() >= 9,
        "registry should have ≥9 FMs (pass-28 baseline), got {}",
        fixers.len()
    );
    assert_eq!(
        v["fixers_count"].as_u64().unwrap_or(0) as usize,
        fixers.len(),
        "fixers_count must match fixers[].length"
    );
    // Every entry must have id/severity/op_pattern/subsystem.
    for f in fixers {
        for required in ["id", "severity", "subsystem", "op_pattern", "auto_fixable"] {
            assert!(
                f.get(required).is_some(),
                "fixer entry missing field `{required}`: {f}"
            );
        }
    }
}

#[test]
fn am_doctor_fix_list_emits_list_all_envelope() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, stdout, stderr) = run_am(td.path(), &["doctor", "fix", "--list", "--json"]);
    assert_eq!(
        code, 0,
        "am doctor fix --list (no --only) must exit 0; stderr: {stderr}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("am doctor fix --list must emit valid JSON");

    // Pass-24 contract: mode + per_fm + skipped + counts.
    assert_eq!(v["mode"], "list_all");
    assert_eq!(v["tool"], "am");
    let per_fm = v["per_fm"]
        .as_array()
        .expect("per_fm must be an array (pass-24 contract)");
    let skipped = v["skipped"]
        .as_array()
        .expect("skipped must be an array (pass-24 contract)");
    assert!(
        v["fm_count"].as_u64().unwrap_or(0) >= 9,
        "fm_count should reflect ≥9 registered FMs"
    );
    // total_findings and total_actions_planned must be numbers.
    assert!(v["total_findings"].is_number());
    assert!(v["total_actions_planned"].is_number());
    // Every per_fm entry has fm_id + severity + subsystem + findings_count.
    for entry in per_fm {
        for required in [
            "fm_id",
            "severity",
            "subsystem",
            "op_pattern",
            "findings_count",
        ] {
            assert!(
                entry.get(required).is_some(),
                "per_fm entry missing field `{required}`: {entry}"
            );
        }
    }
    // Skipped entries (if any) must name the missing field.
    for entry in skipped {
        if entry["reason"] == "missing_input" {
            assert!(
                entry.get("missing_field").is_some(),
                "skipped[missing_input] must name missing_field"
            );
        }
    }
}

#[test]
fn am_doctor_explain_registered_fm_falls_back_to_registry() {
    // Pass-23 contract: explain on a registered FM id with no
    // prior run emits a mode="registry" envelope rather than
    // exiting 64.
    let td = tempfile::TempDir::new().expect("tempdir");
    let fm_id = mcp_agent_mail_cli::doctor::fixers::stale_archive_lock::FM_ID;
    let (code, stdout, stderr) = run_am(td.path(), &["doctor", "explain", fm_id]);
    assert_eq!(
        code, 0,
        "am doctor explain {fm_id} (cold) must exit 0; stderr: {stderr}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("am doctor explain must emit valid JSON");
    assert_eq!(v["mode"], "registry");
    assert_eq!(v["finding_id"], fm_id);
    assert!(
        v["fixer_spec"].is_object(),
        "registry-fallback envelope must include fixer_spec"
    );
    assert_eq!(v["fixer_spec"]["id"], fm_id);
}

#[test]
fn am_doctor_explain_unknown_id_exits_64() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, _stdout, stderr) =
        run_am(td.path(), &["doctor", "explain", "fm-not-a-real-id-pass29"]);
    assert_eq!(code, 64, "unknown id must exit 64; stderr: {stderr}");
}

#[test]
fn am_doctor_fixers_table_format_is_human_readable() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let (code, stdout, stderr) = run_am(td.path(), &["doctor", "fixers", "--format", "table"]);
    assert_eq!(code, 0, "table format must exit 0; stderr: {stderr}");
    // Header row contains known column names.
    assert!(
        stdout.contains("Sev") && stdout.contains("Subsystem") && stdout.contains("Op"),
        "table header must include Sev/Subsystem/Op columns; got:\n{stdout}"
    );
    // The fixer ids must appear in the table body.
    for fm_id in [
        "fm-archive-state-files-stale-archive-lock-from-dead-pid",
        "fm-doctor-state-files-dangling-latest-symlink",
        "fm-db-state-files-world-readable-storage-db",
    ] {
        assert!(
            stdout.contains(fm_id),
            "table output must list {fm_id}; got:\n{stdout}"
        );
    }
}
