//! Pass-15 integration test for `am doctor fix --only <fm-id>`.
//!
//! Drives `fixers::dispatch_only` against a real per-FM detector+fixer
//! in a hermetic tempdir. Avoids the `.doctor/` cwd pollution that a
//! full `handle_fix_only` call would cause; the CLI handler is exercised
//! separately via the clap-parse test in `lib.rs`.
//!
//! The test plants a stale `.git/index.lock` (PID 999_999_999, a
//! guaranteed-dead pid above all known pid_max values), then routes
//! through `dispatch_only`, and asserts the lock is quarantined under
//! `<run_dir>/quarantine/`, that the chokepoint wrote `actions.jsonl`,
//! and that the FM id matches the registry's canonical id.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::fixers::{self, DispatchInputs};
use mcp_agent_mail_cli::doctor::mutate::{Capabilities, MutateContext};
use mcp_agent_mail_cli::doctor::runs::scaffold_run_dir;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

fn build_ctx(td: &tempfile::TempDir, run_id: &str, fm_id: &str) -> MutateContext {
    let run_dir = scaffold_run_dir(td.path(), run_id).expect("scaffold run dir");
    let actions = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("actions.jsonl"))
        .expect("open actions.jsonl");
    MutateContext {
        run_id: run_id.to_string(),
        run_dir: run_dir.clone(),
        capabilities: Capabilities {
            write_scopes: vec![td.path().to_path_buf()],
        },
        actions_file: Mutex::new(actions),
        fixer_id: fm_id.to_string(),
        repo_root: td.path().to_path_buf(),
        dry_run: false,
        start: Instant::now(),
        extra_locks: Vec::new(),
    }
}

fn plant_stale_lock_archive(td: &tempfile::TempDir, slug: &str) -> PathBuf {
    let archive = td.path().join(slug);
    fs::create_dir_all(archive.join(".git")).expect("mkdir .git");
    fs::write(archive.join(".git").join("index.lock"), "999999999\n").expect("plant lock");
    archive
}

#[test]
fn dispatch_only_stale_archive_lock_quarantines_via_mutate() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = plant_stale_lock_archive(&td, "alpha");

    let run_id = "2026-05-11T00-00-00Z__only_test";
    let fm_id = fixers::stale_archive_lock::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: vec![archive.clone()],
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        git_detect: None,
        stale_seconds: fixers::stale_archive_lock::DEFAULT_STALE_SECONDS,
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.fm_id, fm_id);
    assert_eq!(outcome.findings_count, 1);
    assert_eq!(outcome.actions_taken, 1);
    assert_eq!(outcome.actions_skipped, 0);
    assert_eq!(outcome.quarantined_paths.len(), 1);
    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].id, fm_id);

    // Original lock removed from the archive root.
    assert!(
        !archive.join(".git").join("index.lock").exists(),
        "stale lock should have been quarantined"
    );
    // Quarantine destination exists with original contents preserved
    // byte-for-byte (per AGENTS.md RULE 1 — no deletion).
    let q = &outcome.quarantined_paths[0];
    assert!(q.exists(), "quarantined lock missing at {}", q.display());
    let body = fs::read_to_string(q).expect("read quarantined lock");
    assert_eq!(body, "999999999\n");

    // The chokepoint must have recorded the action.
    drop(ctx);
    let actions_path = td
        .path()
        .join(".doctor")
        .join("runs")
        .join(run_id)
        .join("actions.jsonl");
    let actions = fs::read_to_string(&actions_path).expect("read actions.jsonl");
    assert!(
        !actions.trim().is_empty(),
        "actions.jsonl should contain the rename mutation"
    );
}

#[test]
fn dispatch_only_unknown_fm_id_returns_error() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let ctx = build_ctx(&td, "2026-05-11T00-00-00Z__unknown", "unknown");
    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        git_detect: None,
        stale_seconds: 300,
    };
    let result = fixers::dispatch_only("fm-not-a-real-fixer-id-xxx", &ctx, &inputs);
    assert!(matches!(result, Err(fixers::DispatchError::UnknownFm(_))));
}

#[test]
fn dispatch_only_wrong_mcp_url_requires_canonical_url() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let ctx = build_ctx(
        &td,
        "2026-05-11T00-00-00Z__url",
        fixers::wrong_mcp_url_json::FM_ID,
    );
    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        git_detect: None,
        stale_seconds: 300,
    };
    let err = fixers::dispatch_only(fixers::wrong_mcp_url_json::FM_ID, &ctx, &inputs)
        .expect_err("missing input should error");
    assert!(matches!(
        err,
        fixers::DispatchError::MissingInput {
            field: "canonical_mcp_url",
            ..
        }
    ));
}
