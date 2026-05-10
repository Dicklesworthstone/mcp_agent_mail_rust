//! Capabilities contract snapshot test for `am doctor capabilities --json`.
//!
//! Asserts the contract version, schema version, exit-code dictionary,
//! subsystem list, and required-fields presence are stable. A breaking
//! change to any of these REQUIRES a `doctor_contract_version` major bump
//! AND coordinated agent-side updates.
//!
//! Per the world-class-doctor-mode methodology, the capabilities JSON is
//! THE contract agents rely on. Drift is caught by this test.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::capabilities::{SUBSYSTEMS, build_report};

#[test]
fn capabilities_contract_invariants_hold() {
    let report = build_report("test-version".into(), Vec::new());

    // Schema + contract versions
    assert_eq!(report.schema_version, "1.0", "schema_version contract pin");
    assert_eq!(
        report.doctor_contract_version, "1.0",
        "doctor_contract_version pin"
    );

    // Tool identity
    assert_eq!(report.tool, "am");

    // Subsystem list — must be exactly the 11 subsystems Phase 1 archaeology found.
    assert_eq!(
        report.subsystems.len(),
        11,
        "subsystem count is part of contract"
    );
    for sub in SUBSYSTEMS.iter() {
        assert!(
            report.subsystems.contains(sub),
            "missing subsystem in capabilities: {sub}"
        );
    }
    // Specific must-haves
    for required in [
        "db_state_files",
        "archive_state_files",
        "runtime_processes",
        "mcp_config_files",
        "secrets_env_state",
        "guard_install",
        "environment_toolchain",
        "atc_learning_state",
        "search_index_state",
        "identity_contacts_state",
        "share_export_state",
    ] {
        assert!(
            report.subsystems.contains(&required),
            "missing required subsystem: {required}"
        );
    }
}

#[test]
fn capabilities_exit_code_dictionary_is_complete() {
    let report = build_report("test".into(), Vec::new());

    // Every exit code in the world-class kernel must be present.
    for (code, expected_label) in [
        ("0", "success_or_healthy"),
        ("1", "findings_present_no_fix"),
        ("2", "fix_partial"),
        ("3", "fix_failed_rolled_back"),
        ("4", "refused_unsafe"),
        ("5", "concurrency_lost"),
        ("6", "online_required"),
        ("64", "usage_error"),
        ("66", "no_input"),
        ("73", "cant_create"),
        ("74", "io_error"),
    ] {
        let actual = report.exit_codes.get(code).and_then(|v| v.as_str());
        assert_eq!(
            actual,
            Some(expected_label),
            "exit code {code} contract mismatch"
        );
    }
    // No extra exit codes — extension requires a contract version bump.
    assert_eq!(
        report.exit_codes.len(),
        11,
        "exit code count is part of contract"
    );
}

#[test]
fn capabilities_detector_list_is_non_empty() {
    let report = build_report("test".into(), Vec::new());
    assert!(
        !report.detectors.is_empty(),
        "detectors must be non-empty (the existing am doctor surface alone has 30+)"
    );
    assert!(
        report.detectors.len() >= 25,
        "detector count fell below the existing-surface floor; possible regression"
    );
    // Spot-check that each detector has the required fields.
    for det in &report.detectors {
        assert!(!det.id.is_empty(), "detector with empty id");
        assert!(!det.subsystem.is_empty(), "detector with empty subsystem");
        assert!(
            ["P0", "P1", "P2", "P3"].contains(&det.severity),
            "detector severity must be P0..P3, got {}",
            det.severity
        );
    }
}

#[test]
fn capabilities_fixer_list_is_non_empty() {
    let report = build_report("test".into(), Vec::new());
    assert!(!report.fixers.is_empty(), "fixers must be non-empty");
    for fixer in &report.fixers {
        assert!(!fixer.id.is_empty(), "fixer with empty id");
        assert!(
            !fixer.preconditions.is_empty(),
            "fixer {} has empty preconditions",
            fixer.id
        );
        assert!(!fixer.ops.is_empty(), "fixer {} has empty ops", fixer.id);
        // Every op must be one of the 7 canonical variants.
        for op in &fixer.ops {
            assert!(
                [
                    "WriteFile",
                    "AppendFile",
                    "Rename",
                    "Chmod",
                    "DbExec",
                    "DbMigrate",
                    "SymlinkAtomic",
                ]
                .contains(op),
                "fixer {} declares non-canonical op: {}",
                fixer.id,
                op
            );
        }
    }
}

#[test]
fn capabilities_run_artifact_layout_pinned() {
    let report = build_report("test".into(), Vec::new());
    assert_eq!(report.run_artifact_layout.root, ".doctor/");
    assert!(
        report
            .run_artifact_layout
            .per_run_dir
            .contains(".doctor/runs/")
    );
    assert_eq!(
        report.run_artifact_layout.latest_symlink,
        ".doctor/latest -> runs/<run-id>"
    );
    assert_eq!(
        report.run_artifact_layout.history_jsonl,
        ".doctor/scorecard_history.jsonl"
    );
    // Required per-run files
    for required in [
        "report.json",
        "report.md",
        "actions.jsonl",
        "scorecard.json",
        "stderr.log",
        "stdout.json",
        "undo.sh",
    ] {
        assert!(
            report.run_artifact_layout.files.contains(&required),
            "missing per-run artifact: {required}"
        );
    }
}

#[test]
fn capabilities_serializes_to_json_with_schema_version_at_root() {
    let report = build_report("0.2.52".into(), Vec::new());
    let s = serde_json::to_string(&report).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&s).expect("re-parse");

    assert_eq!(v["schema_version"], "1.0");
    assert_eq!(v["doctor_contract_version"], "1.0");
    assert_eq!(v["tool"], "am");
    assert!(v["detectors"].is_array());
    assert!(v["fixers"].is_array());
    assert!(v["exit_codes"].is_object());
    assert!(v["subsystems"].is_array());
    assert_eq!(v["subsystems"].as_array().unwrap().len(), 11);
}
