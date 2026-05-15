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
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
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
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };
    let result = fixers::dispatch_only("fm-not-a-real-fixer-id-xxx", &ctx, &inputs);
    assert!(matches!(result, Err(fixers::DispatchError::UnknownFm(_))));
}

#[test]
fn detect_only_finds_stale_lock_without_touching_chokepoint() {
    // Pass-16: pure detection. Asserts no .doctor/runs/, no
    // actions.jsonl, no quarantine — just findings.
    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = plant_stale_lock_archive(&td, "alpha");

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: vec![archive.clone()],
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome =
        fixers::detect_only(fixers::stale_archive_lock::FM_ID, &inputs).expect("detect_only");
    assert_eq!(outcome.fm_id, fixers::stale_archive_lock::FM_ID);
    assert_eq!(outcome.findings_count, 1);
    assert_eq!(outcome.actions_planned, 1);
    assert_eq!(outcome.findings.len(), 1);

    // Lock must STILL EXIST — detect_only never quarantines.
    let lock_path = archive.join(".git").join("index.lock");
    assert!(
        lock_path.exists(),
        "detect_only must NOT remove the lock — it's read-only"
    );
    // No run-dir scaffolded under the tempdir.
    assert!(
        !td.path().join(".doctor").exists(),
        "detect_only must not create .doctor/ scaffolding"
    );
}

#[test]
fn canonical_stale_seconds_differ_per_fm() {
    // Pass-19 invariant. The three stale-* FMs each declare their own
    // canonical default; the dispatcher routes each FM's detect() call
    // through `inputs.stale_seconds_override.unwrap_or(<this>::DEFAULT)`.
    // If any two collapse to the same value, the routing argument
    // weakens and the prior single-threshold drift bug becomes
    // re-introducable. This test pins the pairwise-distinct invariant.
    let archive = fixers::stale_archive_lock::DEFAULT_STALE_SECONDS;
    let ref_lock = fixers::stale_head_or_ref_lock::DEFAULT_STALE_SECONDS;
    let listener = fixers::stale_listener_pid_hint::DEFAULT_STALE_SECONDS;
    assert_eq!(archive, 300, "archive-lock canonical threshold");
    assert_eq!(ref_lock, 120, "ref-lock canonical threshold (stricter)");
    assert_eq!(listener, 600, "listener-pid canonical threshold (longer)");
    assert_ne!(archive, ref_lock);
    assert_ne!(archive, listener);
    assert_ne!(ref_lock, listener);
}

#[test]
fn detect_only_routes_ref_lock_through_canonical_120s_default() {
    // Plant a HEAD.lock with mtime 200 seconds ago — older than ref-lock's
    // canonical 120s threshold but younger than archive-lock's 300s. With
    // the pre-pass-19 unified-threshold bug, detect_only would have used
    // archive-lock's 300s and returned 0 findings here. Pass-19 routes
    // through each FM's own canonical default, so this returns 1.
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = td.path().join("alpha");
    fs::create_dir_all(archive.join(".git")).expect("mkdir .git");
    let head_lock = archive.join(".git").join("HEAD.lock");
    fs::write(&head_lock, "").expect("plant HEAD.lock");

    // Backdate mtime to 200 seconds ago using std::fs::FileTimes
    // (stable since 1.75; nightly toolchain in this project).
    let two_hundred_secs_ago = SystemTime::now() - Duration::from_secs(200);
    let f = std::fs::File::options()
        .write(true)
        .open(&head_lock)
        .expect("reopen for set_times");
    let times = FileTimes::new()
        .set_accessed(two_hundred_secs_ago)
        .set_modified(two_hundred_secs_ago);
    f.set_times(times).expect("set_times");
    drop(f);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: vec![archive],
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome =
        fixers::detect_only(fixers::stale_head_or_ref_lock::FM_ID, &inputs).expect("detect_only");
    assert_eq!(
        outcome.findings_count, 1,
        "200s-old HEAD.lock must flag with ref-lock's 120s canonical (pre-pass-19 bug would return 0)"
    );

    // Sanity check: feed the SAME archive to archive-lock — its 300s
    // threshold means a 200s-old lock body file shouldn't qualify there.
    // (archive-lock looks for .git/index.lock specifically, which we
    // didn't plant, so the assertion is vacuous but documents intent.)
    let archive_outcome =
        fixers::detect_only(fixers::stale_archive_lock::FM_ID, &inputs).expect("detect_only");
    assert_eq!(
        archive_outcome.findings_count, 0,
        "no .git/index.lock planted → archive-lock returns 0"
    );
}

#[test]
fn dispatch_only_world_readable_storage_db_chmods_via_chokepoint() {
    // Pass-25: 8th FM end-to-end. Plant a world-readable
    // `storage.sqlite3`, route through dispatch_only, assert
    // chmod-to-0o600 and chokepoint records the action.
    use std::os::unix::fs::PermissionsExt;

    let td = tempfile::TempDir::new().expect("tempdir");
    let db = td.path().join("storage.sqlite3");
    fs::write(&db, b"sqlite header").expect("plant db");
    fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).expect("0o644");

    let run_id = "2026-05-13T00-00-00Z__storage_db";
    let fm_id = fixers::world_readable_storage_db::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: vec![db.clone()],
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.fm_id, fm_id);
    assert_eq!(outcome.findings_count, 1);
    assert_eq!(outcome.actions_taken, 1);

    let mode = fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "DB mode must be 0o600 post-fix");

    drop(ctx);
    let actions_path = td
        .path()
        .join(".doctor")
        .join("runs")
        .join(run_id)
        .join("actions.jsonl");
    let actions = fs::read_to_string(&actions_path).expect("read actions.jsonl");
    assert!(
        actions.contains("Chmod"),
        "actions.jsonl must record a Chmod op"
    );
}

#[test]
fn dispatch_only_dangling_doctor_latest_re_aims_via_chokepoint() {
    // Pass-28: 9th FM, Op::SymlinkAtomic at FM level. Plant a
    // dangling `.doctor/latest` plus one surviving runs/<id>
    // directory. Route through dispatch_only and assert the
    // symlink is re-aimed at the surviving target AND the
    // chokepoint records a SymlinkAtomic op in actions.jsonl.
    let td = tempfile::TempDir::new().expect("tempdir");
    let doctor_root = td.path().join(".doctor");
    let runs = doctor_root.join("runs");
    let surviving = "2026-05-13T01-00-00Z__alive";
    fs::create_dir_all(runs.join(surviving)).expect("mkdir surviving run");

    let latest = doctor_root.join("latest");
    let dangling = PathBuf::from("runs").join("2026-05-13T00-00-00Z__gone");
    std::os::unix::fs::symlink(&dangling, &latest).expect("plant dangling symlink");

    let run_id = "2026-05-13T02-00-00Z__symlink_fm";
    let fm_id = fixers::dangling_doctor_latest::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: Some(latest.clone()),
        stale_seconds_override: None,
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.fm_id, fm_id);
    assert_eq!(outcome.findings_count, 1);
    assert_eq!(outcome.actions_taken, 1);

    // The chokepoint also scaffolded its own run-dir
    // (`<td>/.doctor/runs/<run_id>/`) via build_ctx. The fixer
    // picks whichever runs/<id> is newest by mtime, which may
    // be either the planted `surviving` dir or the build_ctx
    // scaffold. The behavioral guarantee we care about is that
    // the symlink no longer dangles.
    let new_target = fs::read_link(&latest).expect("read .doctor/latest");
    let resolved = doctor_root.join(&new_target);
    assert!(
        resolved.exists(),
        "re-aimed target {resolved:?} must exist (no longer dangling); planted surviving={surviving}"
    );
    assert_ne!(
        new_target.to_string_lossy(),
        dangling.to_string_lossy(),
        "symlink must have been updated (not still pointing at the original dangling target)"
    );

    drop(ctx);
    let actions_path = td
        .path()
        .join(".doctor")
        .join("runs")
        .join(run_id)
        .join("actions.jsonl");
    let actions = fs::read_to_string(&actions_path).expect("read actions.jsonl");
    assert!(
        actions.contains("SymlinkAtomic"),
        "actions.jsonl must record a SymlinkAtomic op (got: {actions})"
    );
}

#[test]
fn dispatch_only_dangling_doctor_latest_requires_target_path() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let fm_id = fixers::dangling_doctor_latest::FM_ID;
    let ctx = build_ctx(&td, "2026-05-13T00-00-00Z__no_input", fm_id);
    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };
    let err = fixers::dispatch_only(fm_id, &ctx, &inputs)
        .expect_err("missing input must surface as DispatchError");
    assert!(matches!(
        err,
        fixers::DispatchError::MissingInput {
            field: "doctor_latest_target",
            ..
        }
    ));
}

#[test]
fn dispatch_only_unrelated_fm_does_not_touch_gitignore() {
    // Pass-22: regression test for the chokepoint-bypass that
    // `runs::ensure_gitignore_entry` used to introduce in
    // `handle_fix_only`. Even though pass-21 added the
    // missing_gitignore_entry FM, the side-effect call in the
    // handler meant an unrelated `--only` run (e.g. for
    // stale_archive_lock) would silently mutate `.gitignore`
    // outside the chokepoint. Pass-22 removed the side-effect;
    // this test pins that — only the FM-targeted call is allowed
    // to touch `.gitignore`.
    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = plant_stale_lock_archive(&td, "alpha");
    // Plant a gitignore MISSING `.doctor/`. If the chokepoint
    // bypass returned, the dispatcher would silently append; if
    // it's truly gone, the file stays byte-identical.
    let gi = td.path().join(".gitignore");
    let original_body = "target/\nnode_modules/\n";
    fs::write(&gi, original_body).expect("plant .gitignore");

    let run_id = "2026-05-12T00-00-00Z__no_side_effect";
    let fm_id = fixers::stale_archive_lock::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: vec![archive],
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        // Even with gitignore_target populated, dispatch for an
        // unrelated FM must NOT invoke the gitignore detector.
        gitignore_target: Some(gi.clone()),
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    // The stale-archive-lock FM should run normally.
    assert_eq!(outcome.fm_id, fm_id);
    assert!(outcome.actions_taken >= 1);

    // The .gitignore must be byte-identical to what we planted.
    let body = fs::read_to_string(&gi).expect("read .gitignore");
    assert_eq!(
        body, original_body,
        "unrelated FM dispatch must not mutate .gitignore (pass-22 bypass-removal regression)"
    );
}

#[test]
fn dispatch_only_missing_gitignore_appends_via_chokepoint() {
    // Pass-21: end-to-end exercise of the Op::AppendFile FM. Plant a
    // .gitignore lacking `.doctor/`; route through dispatch_only;
    // assert the chokepoint appended the canonical pattern AND
    // recorded the action in actions.jsonl.
    let td = tempfile::TempDir::new().expect("tempdir");
    let gi = td.path().join(".gitignore");
    fs::write(&gi, "target/\nnode_modules/\n").expect("plant .gitignore");

    let run_id = "2026-05-12T00-00-00Z__gitignore";
    let fm_id = fixers::missing_gitignore_entry::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: Some(gi.clone()),
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.fm_id, fm_id);
    assert_eq!(outcome.findings_count, 1);
    assert_eq!(outcome.actions_taken, 1);
    assert_eq!(outcome.actions_skipped, 0);

    let body = fs::read_to_string(&gi).expect("read .gitignore");
    assert!(body.contains(".doctor/"));
    assert!(body.contains("target/"));

    // Chokepoint must have recorded the action.
    drop(ctx);
    let actions_path = td
        .path()
        .join(".doctor")
        .join("runs")
        .join(run_id)
        .join("actions.jsonl");
    let actions = fs::read_to_string(&actions_path).expect("read actions.jsonl");
    assert!(
        actions.contains("AppendFile"),
        "actions.jsonl must record an AppendFile op (got: {actions})"
    );
}

#[test]
fn dispatch_only_missing_gitignore_requires_target_path() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let fm_id = fixers::missing_gitignore_entry::FM_ID;
    let ctx = build_ctx(&td, "2026-05-12T00-00-00Z__gi_missing", fm_id);

    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };
    let err = fixers::dispatch_only(fm_id, &ctx, &inputs)
        .expect_err("missing input must surface as DispatchError");
    assert!(matches!(
        err,
        fixers::DispatchError::MissingInput {
            field: "gitignore_target",
            ..
        }
    ));
}

#[test]
fn list_all_iterates_registry_and_aggregates_findings() {
    // Pass-24: handle_fix_list_all calls fixers::detect_all to run
    // detect_only for every registered FM and aggregate one envelope.
    // We exercise that shared aggregation directly (the handler is
    // hard to drive in a hermetic test because it resolves cwd /
    // config / storage_root via env). We assert:
    //   - every registered FM is exercised
    //   - findings aggregate without crashing
    //   - MissingInput is structurally surfaceable
    use std::collections::BTreeSet;

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = plant_stale_lock_archive(&td, "alpha");
    let gi = td.path().join(".gitignore");
    fs::write(&gi, "target/\n").expect("plant .gitignore");

    // Build an inputs struct that has SOME findings (stale archive
    // lock + missing .doctor/ entry) and SOME missing inputs
    // (no git_detect, no canonical_mcp_url).
    let inputs = DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: vec![archive],
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: Some(gi),
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };

    let outcome = fixers::detect_all(&inputs).expect("detect_all");
    let mut seen_fm_ids: BTreeSet<String> = BTreeSet::new();
    for entry in &outcome.per_fm {
        seen_fm_ids.insert(entry.fm_id.clone());
    }
    for skipped in &outcome.skipped {
        seen_fm_ids.insert(skipped.fm_id.clone());
        if skipped.reason == "missing_input" {
            assert!(
                skipped.missing_field.is_some_and(|field| !field.is_empty()),
                "MissingInput must name the field"
            );
        }
    }
    // Coverage: every registry id was attempted.
    assert_eq!(outcome.fm_count, fixers::registry().len());
    assert_eq!(seen_fm_ids.len(), fixers::registry().len());
    assert_eq!(
        outcome.per_fm.len() + outcome.skipped.len(),
        outcome.fm_count
    );
    // We planted real failures for at least the stale-archive-lock
    // and missing-gitignore-entry FMs, so findings > 0.
    assert!(
        outcome.total_findings >= 2,
        "expected ≥2 findings (stale lock + gitignore), got {}",
        outcome.total_findings
    );
    // And we deliberately left git_detect=None + canonical_mcp_url=None,
    // so the known-bad-git and wrong-mcp-url FMs report MissingInput.
    assert!(
        outcome
            .skipped
            .iter()
            .filter(|entry| entry.reason == "missing_input")
            .count()
            >= 2,
        "expected ≥2 MissingInput surfacings, got {}",
        outcome.skipped.len()
    );
}

/// Empty `DispatchInputs` that won't satisfy any FM's optional
/// required-input checks. Useful for reachability tests that only
/// care whether the dispatcher recognizes the id, not whether the
/// underlying detector finds anything.
fn empty_inputs(td: &tempfile::TempDir) -> DispatchInputs {
    DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    }
}

#[test]
fn dispatch_only_handles_every_registered_id() {
    // Pass-26 invariant: for every FM in fixers::registry(),
    // `dispatch_only` must recognize the id — i.e. return either
    // Ok(...) or Err(MissingInput {...}). Returning Err(UnknownFm)
    // means the registry entry was added without a matching
    // dispatcher arm, which would silently break `am doctor fix
    // --only <fm-id>` at runtime.
    //
    // This is the only test that catches a "registry-only" FM
    // (registered but not dispatched). Every FM landing in pass-N+1
    // is required to round-trip through this assertion.
    let td = tempfile::TempDir::new().expect("tempdir");
    let inputs = empty_inputs(&td);
    for spec in fixers::registry() {
        let run_id = format!("2026-05-13T00-00-00Z__reach_{}", spec.id);
        let ctx = build_ctx(&td, &run_id, spec.id);
        let result = fixers::dispatch_only(spec.id, &ctx, &inputs);
        match result {
            Ok(_) => {}
            Err(fixers::DispatchError::MissingInput { fm_id, field }) => {
                // MissingInput is fine — it proves the dispatcher
                // recognized the id and reached the input-gate. The
                // field name must be non-empty (pinned by the type
                // signature, but assert here so a future change
                // that uses an empty &'static str fails loudly).
                assert!(
                    !field.is_empty(),
                    "MissingInput must name its required field for {fm_id}"
                );
            }
            Err(fixers::DispatchError::UnknownFm(id)) => panic!(
                "dispatch_only returned UnknownFm({id}) for registered FM `{}` — \
                 registry entry exists but dispatch_only arm is missing",
                spec.id
            ),
            Err(other) => panic!(
                "unexpected dispatch_only error for registered FM `{}`: {other:?}",
                spec.id
            ),
        }
    }
}

#[test]
fn detect_only_handles_every_registered_id() {
    // Pass-26 mirror invariant: detect_only must also have an arm
    // for every registered FM. detect_only doesn't take a
    // MutateContext (it never invokes the chokepoint), so the
    // assertion is simpler than dispatch_only.
    let td = tempfile::TempDir::new().expect("tempdir");
    let inputs = empty_inputs(&td);
    for spec in fixers::registry() {
        let result = fixers::detect_only(spec.id, &inputs);
        match result {
            Ok(_) => {}
            Err(fixers::DispatchError::MissingInput { fm_id, field }) => {
                assert!(
                    !field.is_empty(),
                    "MissingInput must name its required field for {fm_id}"
                );
            }
            Err(fixers::DispatchError::UnknownFm(id)) => panic!(
                "detect_only returned UnknownFm({id}) for registered FM `{}` — \
                 registry entry exists but detect_only arm is missing",
                spec.id
            ),
            Err(other) => panic!(
                "unexpected detect_only error for registered FM `{}`: {other:?}",
                spec.id
            ),
        }
    }
}

#[test]
fn detect_only_unknown_fm_id_returns_error() {
    let inputs = DispatchInputs {
        repo_root: PathBuf::from("/tmp"),
        archive_roots: Vec::new(),
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
    };
    let err =
        fixers::detect_only("fm-also-not-real-xxx", &inputs).expect_err("unknown id should error");
    assert!(matches!(err, fixers::DispatchError::UnknownFm(_)));
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
        canonical_bearer_token: None,
        git_detect: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
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
