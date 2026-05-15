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
