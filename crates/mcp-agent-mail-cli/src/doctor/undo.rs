//! `am doctor undo <run-id>` — restore from `.doctor/runs/<run-id>/backups/`.
//!
//! Reads `actions.jsonl` in reverse order and restores each backup over its
//! target. Verifies post-restore hash matches the recorded `before_hash`.
//! Idempotent: re-running undo on the same run-id is a no-op.
//!
//! Exit codes:
//! - `0` — restore complete (or already complete; idempotent)
//! - `3` — restore failed (a backup was missing or hash didn't match)
//! - `64` — usage error (run-id doesn't exist)
//!
//! Per AGENTS.md, undo cannot resurrect a deleted file. Per `mutate()`'s
//! contract, every file the doctor changes was first verbatim-copied; so
//! undo's only job is to copy backups back.
//!
//! Per AGENTS.md "no file deletion": undo never deletes the run-dir.
//! That's `am doctor gc --before <date> --yes` (separate verb).

#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::runs::doctor_root;

const EMPTY_FILE_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn sha256_hex(bytes: &[u8]) -> String {
    let h = Sha256::digest(bytes);
    format!("sha256:{:x}", h)
}

#[derive(Debug, Deserialize)]
struct StoredAction {
    path: String,
    op: String,
    before_hash: String,
    #[serde(default)]
    rename_to: Option<String>,
    #[serde(default)]
    before_mode: Option<u32>,
    #[serde(default)]
    ok: bool,
}

/// Resolve `<run_id>` argument: literal id OR `latest` (read symlink).
pub fn resolve_run_id(target: &Path, run_id_arg: &str) -> Option<String> {
    if run_id_arg != "latest" {
        return Some(run_id_arg.to_string());
    }
    let latest = doctor_root(target).join("latest");
    let resolved = fs::read_link(&latest).ok()?;
    resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

#[derive(Debug, Default)]
pub struct UndoSummary {
    pub run_id: String,
    pub actions_replayed: usize,
    pub actions_skipped: usize,
    pub failures: Vec<String>,
}

/// Replay `actions.jsonl` in reverse. Restore from `backups/`.
///
/// `dry_run` prints what would happen without writing.
/// `strict` (default true) fails closed if any backup is missing.
pub fn run_undo(target: &Path, run_id: &str, dry_run: bool, strict: bool) -> std::io::Result<UndoSummary> {
    let run_dir = doctor_root(target).join("runs").join(run_id);
    let actions_path = run_dir.join("actions.jsonl");
    let backups_dir = run_dir.join("backups");

    if !actions_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("actions.jsonl not found at {}", actions_path.display()),
        ));
    }

    let mut summary = UndoSummary {
        run_id: run_id.to_string(),
        ..Default::default()
    };

    let f = fs::File::open(&actions_path)?;
    let mut lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
    lines.reverse();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let action: StoredAction = match serde_json::from_str(&line) {
            Ok(a) => a,
            Err(e) => {
                summary
                    .failures
                    .push(format!("could not parse action line: {e}"));
                continue;
            }
        };
        if !action.ok {
            // The mutation failed; mutate() already attempted rollback. Skip.
            summary.actions_skipped += 1;
            continue;
        }

        let target_file = target.join(&action.path);
        let backup_file = backups_dir.join(&action.path);

        match action.op.as_str() {
            "WriteFile" | "AppendFile" | "Chmod" => {
                if !backup_file.exists() {
                    if action.before_hash == EMPTY_FILE_SHA256 {
                        // before_hash of empty file: target didn't exist before
                        // the mutation. Undo by quarantining the new file.
                        let quarantine = run_dir.join("quarantine_undo").join(&action.path);
                        if dry_run {
                            eprintln!("[dry-run] would quarantine new file {}", target_file.display());
                            summary.actions_replayed += 1;
                            continue;
                        }
                        if let Some(parent) = quarantine.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        match fs::rename(&target_file, &quarantine) {
                            Ok(_) => summary.actions_replayed += 1,
                            Err(e) => {
                                if strict {
                                    return Err(e);
                                }
                                summary.failures.push(format!(
                                    "could not quarantine {}: {}",
                                    target_file.display(),
                                    e
                                ));
                            }
                        }
                        continue;
                    }
                    let msg = format!("backup missing for {}", action.path);
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }
                if dry_run {
                    eprintln!("[dry-run] would restore {} from backup", target_file.display());
                    summary.actions_replayed += 1;
                    continue;
                }
                if let Some(parent) = target_file.parent() {
                    fs::create_dir_all(parent)?;
                }
                match fs::copy(&backup_file, &target_file) {
                    Ok(_) => {
                        if let Some(mode) = action.before_mode {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = fs::set_permissions(
                                &target_file,
                                fs::Permissions::from_mode(mode),
                            );
                        }
                        // C2 fix: verify post-restore hash matches before_hash.
                        // If not, the backup is corrupt or tampered — refuse.
                        match fs::read(&target_file) {
                            Ok(bytes) => {
                                let restored_hash = sha256_hex(&bytes);
                                if restored_hash != action.before_hash {
                                    let msg = format!(
                                        "post-restore hash mismatch for {}: expected {}, got {}",
                                        action.path, action.before_hash, restored_hash,
                                    );
                                    if strict {
                                        return Err(std::io::Error::other(msg));
                                    }
                                    summary.failures.push(msg);
                                    continue;
                                }
                                summary.actions_replayed += 1;
                            }
                            Err(e) => {
                                if strict {
                                    return Err(e);
                                }
                                summary.failures.push(format!(
                                    "could not re-read restored {}: {}",
                                    target_file.display(),
                                    e
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary.failures.push(format!(
                            "could not restore {}: {}",
                            target_file.display(),
                            e
                        ));
                    }
                }
            }
            "Rename" => {
                let Some(rename_to) = action.rename_to.as_ref() else {
                    summary
                        .failures
                        .push(format!("Rename action missing rename_to: {}", action.path));
                    continue;
                };
                let from_after = target.join(rename_to);
                let restore_to = target.join(&action.path);
                if dry_run {
                    eprintln!(
                        "[dry-run] would rename back: {} -> {}",
                        from_after.display(),
                        restore_to.display()
                    );
                    summary.actions_replayed += 1;
                    continue;
                }
                if let Some(parent) = restore_to.parent() {
                    fs::create_dir_all(parent)?;
                }
                // H6 fix: refuse if `restore_to` already exists — POSIX rename
                // overwrites silently, which would functionally delete any file
                // the user (or another fixer) recreated at the original path.
                if fs::symlink_metadata(&restore_to).is_ok() {
                    let msg = format!(
                        "undo would clobber existing file at {} (file recreated post-rename); refusing",
                        restore_to.display(),
                    );
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }
                match fs::rename(&from_after, &restore_to) {
                    Ok(_) => summary.actions_replayed += 1,
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary
                            .failures
                            .push(format!("could not reverse rename: {}", e));
                    }
                }
            }
            "SymlinkAtomic" => {
                // Symlinks: undo restores the symlink target from the backup
                // (which itself was a symlink copy if existed, OR records that
                // no symlink existed before).
                if dry_run {
                    eprintln!(
                        "[dry-run] would restore symlink at {}",
                        target_file.display()
                    );
                    summary.actions_replayed += 1;
                    continue;
                }
                // For now, best-effort: remove and recreate from backup.
                // Backup of a symlink is its target string; without that
                // captured, we conservatively skip.
                summary.actions_skipped += 1;
            }
            "DbExec" | "DbMigrate" => {
                // DB-row level undo requires the project's DbConn + a saved
                // .dump of the affected rows. Wired by the dispatch layer
                // in pass-2. For pass-1, mark as skipped (these ops aren't
                // emitted by any pass-1 fixer yet).
                summary.actions_skipped += 1;
            }
            other => {
                summary
                    .failures
                    .push(format!("unknown op kind: {}", other));
            }
        }
    }

    if !dry_run {
        // Mark undo complete by writing a sentinel into the run-dir so
        // subsequent invocations are no-ops (idempotence).
        let sentinel = run_dir.join("undo_complete");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sentinel)?;
        let line = format!(
            "{{\"completed_at\":\"{}\",\"actions_replayed\":{},\"actions_skipped\":{}}}\n",
            super::runs::now_iso_seconds(),
            summary.actions_replayed,
            summary.actions_skipped,
        );
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
    }

    Ok(summary)
}

/// Check if a run-id has already been undone.
pub fn undo_complete(target: &Path, run_id: &str) -> bool {
    let sentinel = doctor_root(target)
        .join("runs")
        .join(run_id)
        .join("undo_complete");
    sentinel.exists()
}

/// Build a list of run-ids available for undo.
pub fn list_undoable_runs(target: &Path) -> std::io::Result<Vec<PathBuf>> {
    let runs_dir = doctor_root(target).join("runs");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("actions.jsonl").exists() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{mutate, Capabilities, MutateContext, Op};
    use crate::doctor::runs::scaffold_run_dir;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn make_ctx(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir,
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: "test-fixer".into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
        }
    }

    #[test]
    fn undo_restores_a_write() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__abc";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"new\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        // Drop ctx so its actions_file flushes and the file handle is released.
        drop(ctx);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    }

    #[test]
    fn undo_dry_run_does_not_write() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__abc";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"new\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        let _ = run_undo(td.path(), run_id, true, true).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "new\n",
            "dry-run must not restore"
        );
    }

    #[test]
    fn undo_records_completion_sentinel() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__abc";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"new\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        assert!(!undo_complete(td.path(), run_id));
        let _ = run_undo(td.path(), run_id, false, true).unwrap();
        assert!(undo_complete(td.path(), run_id));
    }

    #[test]
    fn undo_reverses_a_rename() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        fs::write(&target, b"data\n").unwrap();
        let quarantine = td.path().join("quar").join("victim.txt");
        let run_id = "2026-05-09T16-30-15Z__abc";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::Rename {
                to: quarantine.clone(),
            },
        )
        .unwrap();
        drop(ctx);
        assert!(!target.exists());
        assert!(quarantine.exists());
        let _ = run_undo(td.path(), run_id, false, true).unwrap();
        assert!(target.exists(), "rename should be reversed");
        assert!(!quarantine.exists(), "quarantine should be empty after undo");
    }

    #[test]
    fn resolve_run_id_supports_latest() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-09T16-30-15Z__abc";
        let _ = scaffold_run_dir(td.path(), run_id).unwrap();
        super::super::runs::update_latest_symlink(td.path(), run_id).unwrap();
        let resolved = resolve_run_id(td.path(), "latest").unwrap();
        assert_eq!(resolved, run_id);
    }
}
