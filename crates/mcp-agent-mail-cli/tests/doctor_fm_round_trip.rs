//! Pass-31 — per-FM corrupt → fix → undo → byte-identical round-trip
//! coverage at the FM-dispatch level.
//!
//! The methodology's Phase 9 ("real-world fixture suite") requires
//! that for every auto-fixable FM, the full lifecycle is reversible:
//!
//!   plant corruption  →  capture pre-fix snapshot
//!                     →  dispatch_only(fm_id)
//!                     →  capture post-fix snapshot (different from pre)
//!                     →  run_undo(run-id)
//!                     →  capture post-undo snapshot
//!                     →  assert post-undo == pre-fix BYTE-IDENTICAL
//!
//! The existing `doctor_property_round_trip.rs` exercises this for
//! random `mutate()` Op sequences but does NOT route through the
//! FM-level dispatcher. Pass-31 adds three representative FMs
//! covering three distinct Op patterns (Rename, Chmod, AppendFile).
//! Together they pin the property that the per-FM surface honors the
//! chokepoint's reversibility contract end-to-end.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::fixers::{self, DispatchInputs};
use mcp_agent_mail_cli::doctor::mutate::{Capabilities, MutateContext};
use mcp_agent_mail_cli::doctor::runs::scaffold_run_dir;
use mcp_agent_mail_cli::doctor::undo::run_undo_with_scopes;

/// Round-6 (Gemini F1 P0): `run_undo` now enforces
/// `default_write_scopes()` (which doesn't include /tmp). FM
/// round-trip tests grant the temp dir explicit scope via this
/// shim so the original test intent is preserved.
fn run_undo(
    target: &std::path::Path,
    run_id: &str,
    dry_run: bool,
    strict: bool,
) -> std::io::Result<mcp_agent_mail_cli::doctor::undo::UndoSummary> {
    run_undo_with_scopes(target, run_id, dry_run, strict, &[target.to_path_buf()])
}
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tempfile::TempDir;

/// Per-path (relative-to-`root`) snapshot of (bytes, permission_mode).
/// Sorted by path for stable comparison.
fn snapshot_tree(root: &Path, skip_doctor: bool) -> Vec<(PathBuf, Vec<u8>, u32)> {
    let mut out = Vec::new();
    fn walk(base: &Path, cur: &Path, skip_doctor: bool, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) {
        let entries = match fs::read_dir(cur) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Skip .doctor/ — that's the run-dir scaffold that
            // mutate() created; it's expected to differ across runs
            // (per-mutation seq backups, actions.jsonl, etc.).
            if skip_doctor && path.file_name().and_then(|n| n.to_str()) == Some(".doctor") {
                continue;
            }
            // Skip `.<file>.doctor-lock` artifacts — fs2 advisory
            // locks the chokepoint creates per-path. They persist
            // after the mutation completes (they're a tooling
            // signal, not state) and aren't reverted by undo.
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".doctor-lock")
            {
                continue;
            }
            if meta.file_type().is_dir() {
                walk(base, &path, skip_doctor, out);
            } else if meta.file_type().is_symlink() {
                // For symlinks, snapshot the target string and 0o777
                // as a placeholder (symlink perms aren't portable).
                let target = fs::read_link(&path).unwrap_or_default();
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                out.push((rel, target.into_os_string().into_encoded_bytes(), 0o777));
            } else if meta.file_type().is_file() {
                let bytes = fs::read(&path).unwrap_or_default();
                let mode = meta.permissions().mode() & 0o777;
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                out.push((rel, bytes, mode));
            }
        }
    }
    walk(root, root, skip_doctor, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn build_ctx(td: &TempDir, run_id: &str, fm_id: &str) -> MutateContext {
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

fn empty_inputs(td: &TempDir) -> DispatchInputs {
    DispatchInputs {
        repo_root: td.path().to_path_buf(),
        archive_roots: Vec::new(),
        storage_root: None,
        pid_hint_candidates: Vec::new(),
        token_backup_candidates: Vec::new(),
        mcp_config_candidates: Vec::new(),
        canonical_mcp_url: None,
        canonical_bearer_token: None,
        git_detect: None,
        am_git_binary_detect: None,
        jwt_detect: None,
        port_bind_probe: None,
        gitignore_target: None,
        db_file_candidates: Vec::new(),
        doctor_latest_target: None,
        stale_seconds_override: None,
        missing_project_json_detect_override: None,
        quarantined_bak_detect: None,
    }
}

/// Helper to assert `actual == expected` byte-identical (path,
/// content, mode all match). Emits a structured diff on failure.
fn assert_byte_identical(
    label: &str,
    expected: &[(PathBuf, Vec<u8>, u32)],
    actual: &[(PathBuf, Vec<u8>, u32)],
) {
    if expected == actual {
        return;
    }
    let mut diff = String::new();
    for (path, bytes, mode) in expected {
        let actual_entry = actual.iter().find(|(p, _, _)| p == path);
        match actual_entry {
            None => diff.push_str(&format!(" - missing in actual: {}\n", path.display())),
            Some((_, abytes, amode)) => {
                if abytes != bytes {
                    diff.push_str(&format!(
                        " - byte diff: {} ({} bytes expected, {} actual)\n",
                        path.display(),
                        bytes.len(),
                        abytes.len()
                    ));
                }
                if amode != mode {
                    diff.push_str(&format!(
                        " - mode diff: {} (0o{:o} expected, 0o{:o} actual)\n",
                        path.display(),
                        mode,
                        amode
                    ));
                }
            }
        }
    }
    for (path, _, _) in actual {
        if !expected.iter().any(|(p, _, _)| p == path) {
            diff.push_str(&format!(" - extra in actual: {}\n", path.display()));
        }
    }
    panic!("{label} round-trip not byte-identical:\n{diff}");
}

#[test]
fn round_trip_stale_archive_lock_op_rename() {
    // Plant a stale archive lock → fix (quarantine via Op::Rename) →
    // undo (restore the lock byte-identical).
    let td = TempDir::new().expect("tempdir");
    let archive = td.path().join("alpha_archive");
    fs::create_dir_all(archive.join(".git")).expect("mkdir .git");
    let lock_path = archive.join(".git").join("index.lock");
    fs::write(&lock_path, "999999999\n").expect("plant lock");

    let pre_fix = snapshot_tree(td.path(), true);
    assert!(
        pre_fix.iter().any(|(p, _, _)| p.ends_with("index.lock")),
        "pre-fix snapshot must include the planted lock"
    );

    let run_id = "2026-05-14T00-00-00Z__rt_archive_lock";
    let fm_id = fixers::stale_archive_lock::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        archive_roots: vec![archive.clone()],
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.actions_taken, 1);

    // Post-fix: lock is no longer at the original location.
    assert!(!lock_path.exists(), "fix must have quarantined the lock");

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo must succeed: failures={:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("stale_archive_lock", &pre_fix, &post_undo);
}

#[test]
fn round_trip_world_readable_token_bak_op_chmod() {
    // Plant a token-bearing .bak with world-readable mode →
    // chmod to 0o600 → undo (restores the original 0o644).
    let td = TempDir::new().expect("tempdir");
    let bak = td.path().join("config.toml.bak");
    fs::write(&bak, b"HTTP_BEARER_TOKEN=secret-pass31\n").expect("plant bak");
    fs::set_permissions(&bak, fs::Permissions::from_mode(0o644)).expect("0o644");

    let pre_fix = snapshot_tree(td.path(), true);
    let pre_mode = pre_fix
        .iter()
        .find(|(p, _, _)| p == &PathBuf::from("config.toml.bak"))
        .map(|(_, _, m)| *m)
        .unwrap();
    assert_eq!(pre_mode, 0o644, "pre-fix mode must be 0o644");

    let run_id = "2026-05-14T00-00-00Z__rt_token_bak";
    let fm_id = fixers::world_readable_token_bak::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        token_backup_candidates: vec![bak.clone()],
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.actions_taken, 1);

    let post_fix_mode = fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
    assert_eq!(post_fix_mode, 0o600, "post-fix mode must be 0o600");

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("world_readable_token_bak", &pre_fix, &post_undo);
}

#[test]
fn round_trip_wrong_mcp_url_json_op_write_file() {
    // Plant an MCP client JSON config pointing at the wrong URL →
    // fix (re-write the URL via Op::WriteFile) →
    // undo (restore the original JSON byte-identical).
    let td = TempDir::new().expect("tempdir");
    let cfg = td.path().join("mcp.json");
    let original_body = r#"{
  "mcpServers": {
    "agent-mail": {
      "url": "http://127.0.0.1:9999/mcp/"
    },
    "other": {
      "url": "http://example.com/other"
    }
  }
}
"#;
    fs::write(&cfg, original_body).expect("plant mcp.json");

    let pre_fix = snapshot_tree(td.path(), true);
    let pre_body = pre_fix
        .iter()
        .find(|(p, _, _)| p == &PathBuf::from("mcp.json"))
        .map(|(_, b, _)| b.clone())
        .unwrap();
    assert_eq!(pre_body, original_body.as_bytes());

    let run_id = "2026-05-14T00-00-00Z__rt_mcp_url";
    let fm_id = fixers::wrong_mcp_url_json::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let canonical = "http://127.0.0.1:8765/mcp/";
    let inputs = DispatchInputs {
        mcp_config_candidates: vec![cfg.clone()],
        canonical_mcp_url: Some(canonical.to_string()),
        canonical_bearer_token: None,
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.actions_taken, 1);

    let post_fix_body = fs::read_to_string(&cfg).unwrap();
    assert!(
        post_fix_body.contains("127.0.0.1:8765"),
        "post-fix body must contain canonical URL; got:\n{post_fix_body}"
    );
    assert_ne!(
        post_fix_body, original_body,
        "post-fix body must differ from original"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("wrong_mcp_url_json", &pre_fix, &post_undo);
}

#[test]
fn round_trip_dangling_doctor_latest_op_symlink_atomic() {
    // Plant a dangling `.doctor/latest` symlink + a surviving
    // runs/<id> → fix (re-aim via Op::SymlinkAtomic) →
    // undo (restore the original dangling symlink byte-for-byte).
    //
    // The FM under test resolves `runs_dir` from the symlink's
    // parent, which is `<td>/.doctor/`. We isolate the FM's tree
    // under `<td>/repo/` so the chokepoint's run-dir (created at
    // `<td>/.doctor/runs/<run_id>/` by build_ctx) doesn't bleed
    // into the FM's discovery scan — same isolation pattern as
    // the pass-28 module test.
    let td = TempDir::new().expect("tempdir");
    let isolated = td.path().join("repo");
    let doctor_root = isolated.join(".doctor");
    let runs = doctor_root.join("runs");
    let surviving = "2026-05-14T00-00-00Z__alive";
    fs::create_dir_all(runs.join(surviving)).expect("mkdir surviving run");

    let latest = doctor_root.join("latest");
    let dangling = PathBuf::from("runs").join("2026-05-14T00-00-00Z__gone");
    std::os::unix::fs::symlink(&dangling, &latest).expect("plant dangling symlink");

    // Snapshot the isolated tree (not td.path() — that includes
    // the chokepoint's `<td>/.doctor/` scaffold).
    let pre_fix = snapshot_tree(&isolated, false);
    let pre_link = pre_fix
        .iter()
        .find(|(p, _, _)| p == &PathBuf::from(".doctor/latest"))
        .map(|(_, b, _)| b.clone())
        .expect("pre-fix snapshot must capture the dangling symlink");
    let dangling_bytes = dangling.clone().into_os_string().into_encoded_bytes();
    assert_eq!(
        pre_link, dangling_bytes,
        "pre-fix symlink target must match what we planted"
    );

    let run_id = "2026-05-14T00-00-00Z__rt_dangling";
    let fm_id = fixers::dangling_doctor_latest::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        doctor_latest_target: Some(latest.clone()),
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.actions_taken, 1);

    // Post-fix: symlink re-aimed; no longer dangling.
    let post_fix_target = fs::read_link(&latest).expect("read symlink");
    assert_ne!(
        post_fix_target, dangling,
        "fix must have updated the symlink target"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(&isolated, false);
    assert_byte_identical("dangling_doctor_latest", &pre_fix, &post_undo);
}

/// Pass-35CP+Q: FM `fm-archive-state-files-missing-or-malformed-
/// project-json` partial-graduated to `Op::WriteFile`. Pin the
/// corrupt → fix → undo → byte-identical-content contract using
/// the test-only `missing_project_json_detect_override` hook to
/// inject a synthetic anomaly report (avoids needing a real DB +
/// archive for the round-trip).
#[test]
fn round_trip_missing_or_malformed_project_json_op_write_file() {
    use mcp_agent_mail_db::archive_anomaly::{
        ArchiveAnomaly, ArchiveAnomalyKind, ArchiveAnomalyReport,
    };
    let td = TempDir::new().expect("tempdir");
    let project_json = td.path().join("malformed_project.json");
    let malformed = r#"{"slug": "demo", "bad-key": "x"#;
    fs::write(&project_json, malformed).expect("plant malformed project.json");
    fs::set_permissions(&project_json, fs::Permissions::from_mode(0o644)).expect("0o644");

    let pre_fix = snapshot_tree(td.path(), true);

    // Build a synthetic report with one InvalidProjectMetadata
    // entry whose `canonical_human_key` is Some(absolute) — the
    // exact shape that pass-35CP auto-fixes.
    let mut report = ArchiveAnomalyReport::new();
    report.anomalies.push(ArchiveAnomaly::now(
        ArchiveAnomalyKind::InvalidProjectMetadata {
            path: project_json.clone(),
            slug: "demo".to_string(),
            canonical_human_key: Some("/workspaces/demo".to_string()),
            detail: "unterminated string literal".to_string(),
        },
    ));

    let run_id = "2026-05-17T00-00-00Z__rt_project_json";
    let fm_id = fixers::missing_or_malformed_project_json::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        missing_project_json_detect_override: Some(
            fixers::missing_or_malformed_project_json::DetectInputs {
                storage_root_override: Some(td.path().to_path_buf()),
                report_override: Some(report),
            },
        ),
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one rewrite expected for the Invalid-with-canonical entry"
    );

    // Post-fix: the file is the canonical {slug, human_key} JSON.
    let post_fix_body = fs::read_to_string(&project_json).expect("read post-fix");
    let post_value: serde_json::Value =
        serde_json::from_str(&post_fix_body).expect("parse post-fix");
    assert_eq!(
        post_value.get("slug").and_then(|v| v.as_str()),
        Some("demo")
    );
    assert_eq!(
        post_value.get("human_key").and_then(|v| v.as_str()),
        Some("/workspaces/demo")
    );
    // The malformed `bad-key` entry from the original is GONE
    // (this is a CLEAN reconstruction — only canonical fields).
    assert!(
        post_value.get("bad-key").is_none(),
        "auto-fix produces clean canonical shape; sibling junk is dropped"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("missing_or_malformed_project_json", &pre_fix, &post_undo);
}

/// Pass-35CL: FM `fm-identity_contacts_state-build-slot-lease-expired`
/// graduated from detect-only to `Op::WriteFile`. Pin the corrupt
/// → fix → undo → byte-identical-content contract for ghost-lease
/// `released_ts` rewrite. UPDATE-only: the file is never deleted
/// per RULE 1; sibling JSON fields survive verbatim.
#[test]
fn round_trip_identity_build_slot_lease_expired_op_write_file() {
    let td = TempDir::new().expect("tempdir");
    let storage_root = td.path().join("storage");
    let lease_dir = storage_root
        .join("projects")
        .join("demo")
        .join("build_slots")
        .join("build-1");
    fs::create_dir_all(&lease_dir).expect("mkdir lease dir");
    let lease_path = lease_dir.join("GhostHolder.json");
    // Plant a lease that expired in 2020 with non-trivial sibling
    // fields. The detector's wall-clock `now_iso` will be > 2026,
    // so it'll flag this regardless of the exact run timestamp.
    let original_body = r#"{"expires_ts":"2020-01-01T00:00:00Z","released_ts":null,"acquired_ts":"2019-12-31T23:00:00Z","holder":"GhostHolder","slot_metadata":{"label":"build-alpha","priority":7}}"#;
    fs::write(&lease_path, original_body).expect("plant lease");
    fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o644)).expect("0o644");

    let pre_fix = snapshot_tree(td.path(), true);

    let run_id = "2026-05-16T00-00-00Z__rt_lease";
    let fm_id = fixers::identity_build_slot_lease_expired::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        storage_root: Some(storage_root.clone()),
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one rewrite expected for the ghost lease"
    );

    // Post-fix: released_ts must be non-null, all sibling fields
    // preserved verbatim.
    let post_fix_body = fs::read_to_string(&lease_path).expect("read post-fix");
    let post_value: serde_json::Value =
        serde_json::from_str(&post_fix_body).expect("parse post-fix");
    assert!(
        post_value.get("released_ts").is_some_and(|r| r.is_string()),
        "released_ts must be a string after fix"
    );
    assert_eq!(
        post_value.get("holder").and_then(|v| v.as_str()),
        Some("GhostHolder"),
        "holder sibling field must be preserved verbatim"
    );
    assert_eq!(
        post_value
            .get("slot_metadata")
            .and_then(|m| m.get("priority"))
            .and_then(|v| v.as_u64()),
        Some(7),
        "nested sibling field must be preserved verbatim"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("identity_build_slot_lease_expired", &pre_fix, &post_undo);
}

#[test]
fn round_trip_missing_gitignore_entry_op_append_file() {
    // Plant a .gitignore lacking `.doctor/` → fix (append) →
    // undo (restore the original file byte-identical).
    let td = TempDir::new().expect("tempdir");
    let gi = td.path().join(".gitignore");
    let original_body = "target/\nnode_modules/\n";
    fs::write(&gi, original_body).expect("plant .gitignore");

    let pre_fix = snapshot_tree(td.path(), true);
    let pre_body = pre_fix
        .iter()
        .find(|(p, _, _)| p == &PathBuf::from(".gitignore"))
        .map(|(_, b, _)| b.clone())
        .unwrap();
    assert_eq!(pre_body, original_body.as_bytes());

    let run_id = "2026-05-14T00-00-00Z__rt_gitignore";
    let fm_id = fixers::missing_gitignore_entry::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        gitignore_target: Some(gi.clone()),
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(outcome.actions_taken, 1);

    let post_fix_body = fs::read_to_string(&gi).unwrap();
    assert!(
        post_fix_body.contains(".doctor/"),
        "post-fix .gitignore must contain `.doctor/`"
    );
    assert_ne!(
        post_fix_body, original_body,
        "post-fix body must differ from original"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("missing_gitignore_entry", &pre_fix, &post_undo);
}

/// Pass-35CJ: FM17 graduated from detect-only to `Op::Chmod`
/// auto-fix. Pin the corrupt → fix → undo → byte-identical-mode
/// contract for the guard pre-commit hook chain runner.
#[test]
fn round_trip_guard_plugin_not_executable_op_chmod() {
    // Plant a git repo with the agent-mail pre-commit chain runner
    // at 0o644 (missing user-exec bit). The detector will flag it;
    // fix() chmods to 0o755 via the chokepoint; undo() restores
    // the original 0o644.
    let td = TempDir::new().expect("tempdir");
    git2::Repository::init(td.path()).expect("git init");
    let hooks_dir = td.path().join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("mkdir .git/hooks");

    let pre_commit = hooks_dir.join("pre-commit");
    // Body must contain an agent-mail sentinel so the detector
    // recognizes it as ours (not an unrelated foreign hook).
    let body = "#!/bin/sh\n# mcp-agent-mail chain-runner (pre-commit)\nexit 0\n";
    fs::write(&pre_commit, body).expect("plant pre-commit");
    fs::set_permissions(&pre_commit, fs::Permissions::from_mode(0o644))
        .expect("set 0o644 on pre-commit");

    let pre_fix = snapshot_tree(td.path(), true);
    let pre_mode = pre_fix
        .iter()
        .find(|(p, _, _)| p.ends_with("hooks/pre-commit"))
        .map(|(_, _, m)| *m)
        .expect("pre-fix snapshot must contain pre-commit");
    assert_eq!(pre_mode, 0o644, "pre-fix mode must be 0o644");

    let run_id = "2026-05-16T00-00-00Z__rt_guard_chmod";
    let fm_id = fixers::guard_plugin_not_executable::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = empty_inputs(&td);

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one chmod expected for the planted pre-commit"
    );

    let post_fix_mode = fs::metadata(&pre_commit).unwrap().permissions().mode() & 0o7777;
    assert_eq!(post_fix_mode, 0o755, "post-fix mode must be 0o755");

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("guard_plugin_not_executable", &pre_fix, &post_undo);
}

/// Pass-35CK: FM `fm-mcp-config-files-quarantined-bak-files-with-tokens`
/// graduated from detect-only to `Op::Chmod`. Pin the corrupt →
/// fix → undo → byte-identical-mode contract on a timestamped
/// MCP-config backup carrying token-shape content.
#[test]
fn round_trip_quarantined_bak_files_op_chmod() {
    // Plant a timestamped MCP-config backup (`config.json.<ts>.bak`)
    // with token-shape content at mode 0o644. The detector will
    // flag it; fix() chmods to 0o600 via the chokepoint; undo()
    // restores the original 0o644.
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("Claude");
    fs::create_dir_all(&config_dir).expect("mkdir Claude config dir");
    let bak = config_dir.join("claude_desktop_config.json.20260101_120000.bak");
    fs::write(&bak, r#"{"authorization":"Bearer secret-pass35ck"}"#).expect("plant bak");
    fs::set_permissions(&bak, fs::Permissions::from_mode(0o644)).expect("set 0o644");

    let pre_fix = snapshot_tree(td.path(), true);
    let pre_mode = pre_fix
        .iter()
        .find(|(p, _, _)| p.ends_with("claude_desktop_config.json.20260101_120000.bak"))
        .map(|(_, _, m)| *m)
        .expect("pre-fix snapshot must contain the bak");
    assert_eq!(pre_mode, 0o644, "pre-fix mode must be 0o644");

    let run_id = "2026-05-16T00-00-00Z__rt_qbak";
    let fm_id = fixers::quarantined_bak_files::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        quarantined_bak_detect: Some(fixers::quarantined_bak_files::DetectInputs {
            dir_overrides: Some(vec![config_dir.clone()]),
        }),
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one chmod expected for the planted bak"
    );

    let post_fix_mode = fs::metadata(&bak).unwrap().permissions().mode() & 0o7777;
    assert_eq!(post_fix_mode, 0o600, "post-fix mode must be 0o600");

    // The file content must be untouched (chmod, not rename or
    // rewrite). Snapshot diff after undo proves byte-identity.
    let mid_fix_content = fs::read_to_string(&bak).unwrap();
    assert!(
        mid_fix_content.contains("secret-pass35ck"),
        "fix must not modify file content"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("quarantined_bak_files", &pre_fix, &post_undo);
}

/// FM `fm-mcp-config-files-duplicate-aliased-server-entries`
/// graduated from detect-only to `Op::WriteFile` (partial). Pin
/// the corrupt → fix → undo → byte-identical-content contract:
/// plant a `.json` MCP config with canonical
/// `(mcpServers, mcp-agent-mail)` plus two non-canonical
/// duplicates, dispatch, verify only canonical remains, undo,
/// verify all 3 entries are back byte-identical.
#[test]
fn round_trip_mcp_duplicate_aliased_server_entries_op_write_file() {
    let td = TempDir::new().expect("tempdir");
    let cfg = td.path().join("claude.json");
    // Three agent-mail registrations: canonical + 2 duplicates.
    let original = r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "url": "http://canonical",
      "args": ["serve"]
    },
    "other-server": {
      "url": "http://other"
    }
  },
  "servers": {
    "mcp_agent_mail": {
      "url": "http://stale-1"
    }
  },
  "mcp": {
    "agent-mail": {
      "url": "http://stale-2"
    }
  }
}"#;
    fs::write(&cfg, original).expect("plant config");
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o644)).expect("0o644");

    let pre_fix = snapshot_tree(td.path(), true);

    let run_id = "2026-05-19T00-00-00Z__rt_dup";
    let fm_id = fixers::mcp_duplicate_aliased_server_entries::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        mcp_config_candidates: vec![cfg.clone()],
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one rewrite expected for the config with canonical+2 duplicates"
    );

    // Post-fix: canonical entry preserved, both duplicates gone.
    let post_value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).expect("parse post-fix");
    assert_eq!(
        post_value
            .pointer("/mcpServers/mcp-agent-mail/url")
            .and_then(|v| v.as_str()),
        Some("http://canonical"),
        "canonical entry URL preserved"
    );
    assert_eq!(
        post_value
            .pointer("/mcpServers/mcp-agent-mail/args/0")
            .and_then(|v| v.as_str()),
        Some("serve"),
        "canonical entry nested args preserved"
    );
    assert_eq!(
        post_value
            .pointer("/mcpServers/other-server/url")
            .and_then(|v| v.as_str()),
        Some("http://other"),
        "sibling server preserved"
    );
    assert!(
        post_value.pointer("/servers/mcp_agent_mail").is_none(),
        "non-canonical (servers, mcp_agent_mail) removed"
    );
    assert!(
        post_value.pointer("/mcp/agent-mail").is_none(),
        "non-canonical (mcp, agent-mail) removed"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("mcp_duplicate_aliased_server_entries", &pre_fix, &post_undo);
}

/// FM `fm-db-state-files-legacy-fts-residue` graduated from
/// detect-only to `Op::DbExec`. FIRST Op::DbExec round-trip in
/// the suite — pins that the chokepoint's whole-DB-file backup
/// reverses a `DROP` sequence byte-identically.
///
/// Plant a delete-journal-mode SQLite DB with a legacy
/// `fts_messages` table + `fts_messages_ai` trigger plus the
/// Search-V3 `.managed.json` marker. Dispatch → fix drops the
/// residue via Op::DbExec → detector confirms clean → undo
/// restores the DB byte-identical (and leaves no stray
/// `-wal`/`-journal` sidecars in the snapshot).
#[test]
fn round_trip_legacy_fts_residue_op_db_exec() {
    use sqlmodel_sqlite::SqliteConnection;

    let td = TempDir::new().expect("tempdir");
    let db_path = td.path().join("storage.sqlite3");
    {
        let conn = SqliteConnection::open_file(db_path.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);")
            .expect("create main table");
        // Legacy FTS5 residue: a table + trigger matching the
        // canonical fts_messages prefix. (Regular table stands in
        // for the FTS5 virtual table — the detector matches by
        // name, and in-process SQLite test builds may lack FTS5.)
        conn.execute_raw("CREATE TABLE fts_messages (rowid INTEGER, content TEXT);")
            .expect("create fts_messages");
        conn.execute_raw(
            "CREATE TRIGGER fts_messages_ai AFTER INSERT ON messages BEGIN \
             INSERT INTO fts_messages(rowid, content) VALUES (NEW.id, NEW.body); END;",
        )
        .expect("create fts trigger");
        // conn drops here → DB closed cleanly, delete-journal mode
        // leaves no sidecar.
    }
    // Search V3 marker → detector treats fts_* as residue.
    let marker_dir = td.path().join("search_index");
    fs::create_dir_all(&marker_dir).expect("mkdir search_index");
    fs::write(marker_dir.join(".managed.json"), b"{}").expect("write marker");

    let pre_fix = snapshot_tree(td.path(), true);

    let run_id = "2026-05-20T00-00-00Z__rt_fts";
    let fm_id = fixers::legacy_fts_residue::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let inputs = DispatchInputs {
        db_file_candidates: vec![db_path.clone()],
        ..empty_inputs(&td)
    };

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one Op::DbExec drop sequence expected"
    );

    // Post-fix: detector finds zero residue.
    let post = fixers::legacy_fts_residue::detect(std::slice::from_ref(&db_path));
    assert!(
        post.is_empty(),
        "detector must find zero residue after fix: {post:?}"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("legacy_fts_residue", &pre_fix, &post_undo);

    // And the residue is back after undo (the DROP was reversed).
    let post_undo_detect = fixers::legacy_fts_residue::detect(std::slice::from_ref(&db_path));
    assert_eq!(
        post_undo_detect.len(),
        1,
        "residue must reappear after undo restores the DB"
    );
}

/// FM `fm-share_export_state-half-finished-bundle-after-crash`
/// graduated from detect-only to directory `Op::Rename`. This is
/// the FIRST round-trip exercising the chokepoint's directory
/// quarantine path — pins that a whole debris DIRECTORY TREE is
/// moved (never deleted), then restored byte-identical by undo.
///
/// Plant a partial bundle (`manifest.json` present, no
/// `mailbox.sqlite3`) under `<repo>/archived_mailbox_states/`,
/// dispatch → fix quarantines the dir tree into the run-dir →
/// undo renames it back → assert byte-identical (the bundle dir +
/// its nested files reappear at the original path).
#[test]
fn round_trip_share_half_finished_bundle_op_rename_dir() {
    let td = TempDir::new().expect("tempdir");
    // Partial bundle: manifest.json with no mailbox.sqlite3 payload.
    // Name must NOT match an `am-share-*` temp prefix (those are the
    // stale-temp-dir family, gated on mtime).
    let bundle = td
        .path()
        .join("archived_mailbox_states")
        .join("export-2026-05-20");
    fs::create_dir_all(bundle.join("attachments")).expect("mkdir bundle");
    fs::write(bundle.join("manifest.json"), b"{\"attachments\":1}").expect("manifest");
    fs::write(
        bundle.join("attachments").join("a.bin"),
        b"\xde\xad\xbe\xef",
    )
    .expect("attachment");

    let pre_fix = snapshot_tree(td.path(), true);

    let run_id = "2026-05-20T00-00-00Z__rt_share";
    let fm_id = fixers::share_half_finished_bundle::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    // The dispatcher reads project_root from inputs.repo_root, which
    // build_ctx/empty_inputs both set to td.path().
    let inputs = empty_inputs(&td);

    let outcome = fixers::dispatch_only(fm_id, &ctx, &inputs).expect("dispatch_only");
    assert_eq!(
        outcome.actions_taken, 1,
        "exactly one partial-bundle dir quarantined"
    );

    // Post-fix: the bundle dir is moved out of archived_mailbox_states.
    assert!(
        !bundle.exists(),
        "partial bundle dir should have been quarantined (moved)"
    );

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    // Bundle dir + nested attachment restored byte-identical.
    assert!(bundle.is_dir(), "bundle dir restored by undo");
    assert_eq!(
        fs::read(bundle.join("attachments").join("a.bin")).unwrap(),
        b"\xde\xad\xbe\xef",
        "nested attachment restored byte-identical"
    );

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("share_half_finished_bundle", &pre_fix, &post_undo);
}

/// FM `fm-mcp-config-files-codex-startup-timeout-too-short`
/// graduated from detect-only to `Op::WriteFile` (format-preserving
/// toml_edit). Unlike the other round-trips this drives `fix()`
/// directly rather than `dispatch_only`, because the codex
/// dispatcher reads global config locations
/// (`detect_mcp_config_locations_default` → home dir + CWD) and
/// has no per-test injection point. The property under test is the
/// same: corrupt → fix → undo → byte-identical, here proving the
/// chokepoint reverses a toml_edit-produced rewrite (comments and
/// all) byte-for-byte.
#[test]
fn round_trip_codex_startup_timeout_op_write_file() {
    let td = TempDir::new().expect("tempdir");
    let cfg = td.path().join("config.toml");
    let original = "# codex config (operator comment)\n\
                    [mcp_servers.mcp_agent_mail]\n\
                    command = \"mcp-agent-mail\"  # rust binary\n\
                    startup_timeout_sec = 5\n";
    fs::write(&cfg, original).expect("plant config.toml");
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o644)).expect("0o644");

    let pre_fix = snapshot_tree(td.path(), true);

    let run_id = "2026-05-20T00-00-00Z__rt_codex";
    let fm_id = fixers::codex_startup_timeout::FM_ID;
    let ctx = build_ctx(&td, run_id, fm_id);

    let finding = fixers::codex_startup_timeout::CodexStartupTimeoutFinding {
        config_path: cfg.clone(),
        state: fixers::codex_startup_timeout::TimeoutState::TooShort { observed_secs: 5 },
        min_required_secs: 30,
    };
    let outcome = fixers::codex_startup_timeout::fix(&ctx, &finding).expect("fix");
    assert_eq!(outcome.actions_taken, 1, "the too-short timeout is bumped");

    // Post-fix: timeout bumped, comments preserved.
    let post_fix = fs::read_to_string(&cfg).unwrap();
    assert!(post_fix.contains("startup_timeout_sec = 30"));
    assert!(post_fix.contains("# codex config (operator comment)"));
    assert!(post_fix.contains("# rust binary"));

    drop(ctx);

    let summary = run_undo(td.path(), run_id, false, true).expect("run_undo");
    assert!(
        summary.failures.is_empty(),
        "undo failures: {:?}",
        summary.failures
    );

    // Undo restores the original byte-for-byte (timeout back to 5,
    // comments intact).
    let post_undo_body = fs::read_to_string(&cfg).unwrap();
    assert_eq!(post_undo_body, original, "undo restores original bytes");

    let post_undo = snapshot_tree(td.path(), true);
    assert_byte_identical("codex_startup_timeout", &pre_fix, &post_undo);
}
