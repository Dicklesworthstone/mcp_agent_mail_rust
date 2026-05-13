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

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::runs::doctor_root;

const EMPTY_FILE_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn sha256_hex(bytes: &[u8]) -> String {
    let h = Sha256::digest(bytes);
    format!("sha256:{:x}", h)
}

fn sha256_path_bytes(path: &Path) -> String {
    sha256_hex(path.as_os_str().as_bytes())
}

fn symlink_target_hash(path: &Path) -> std::io::Result<String> {
    Ok(sha256_path_bytes(&fs::read_link(path)?))
}

fn path_from_raw_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

fn read_regular_file_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    read_regular_file_no_follow_inner(path)
}

#[cfg(unix)]
fn read_regular_file_no_follow_inner(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_regular_file_no_follow_inner(path: &Path) -> std::io::Result<Vec<u8>> {
    fs::read(path)
}

#[derive(Debug, Clone, Deserialize)]
struct StoredAction {
    path: String,
    op: String,
    before_hash: String,
    /// SHA-256 of the file's bytes immediately AFTER the mutation. Used by
    /// undo to verify the file is still in the post-mutation state before
    /// restoring (C3 fix). If the user manually modified the file
    /// post-fix, undo refuses to clobber their changes.
    #[serde(default)]
    after_hash: String,
    /// Pass-4 fix: when the same file is mutated multiple times in one run,
    /// each mutation gets its own backup at `backups/seq_<started_at_ns>/<rel>`.
    /// Undo uses this to find the correct per-mutation backup.
    #[serde(default)]
    started_at_ns: u128,
    /// Pass-5 G-Crash-Window fix: `"pending"` (mutation in flight; backup
    /// exists; mutation may or may not have happened yet) or `"completed"`
    /// (mutation finished). Absent = legacy / completed.
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    rename_to: Option<String>,
    #[serde(default)]
    before_mode: Option<u32>,
    #[serde(default)]
    ok: bool,
}

fn per_mutation_backup_dir(backups_dir: &Path, started_at_ns: u128) -> PathBuf {
    backups_dir.join(format!("seq_{:026}", started_at_ns))
}

fn logged_path_error(logged_path: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("unsafe path in doctor action log: {logged_path:?}"),
    )
}

fn checked_logged_components(path: &Path, logged_path: &str) -> std::io::Result<Vec<OsString>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(logged_path_error(logged_path));
            }
        }
    }
    if parts.is_empty() {
        return Err(logged_path_error(logged_path));
    }
    Ok(parts)
}

fn logged_target_path(target: &Path, logged_path: &str) -> std::io::Result<PathBuf> {
    let path = Path::new(logged_path);
    checked_logged_components(path, logged_path)?;
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(target.join(path))
    }
}

fn artifact_relative_path(logged_path: &str) -> std::io::Result<PathBuf> {
    let path = Path::new(logged_path);
    let parts = checked_logged_components(path, logged_path)?;
    let mut out = if path.is_absolute() {
        PathBuf::from("__abs__")
    } else {
        PathBuf::new()
    };
    for part in parts {
        out.push(part);
    }
    Ok(out)
}

fn action_backup_file(backups_dir: &Path, action: &StoredAction) -> std::io::Result<PathBuf> {
    let rel = artifact_relative_path(&action.path)?;
    if action.started_at_ns == 0 {
        Ok(backups_dir.join(rel))
    } else {
        Ok(per_mutation_backup_dir(backups_dir, action.started_at_ns).join(rel))
    }
}

fn run_artifact_path(run_dir: &Path, kind: &str, logged_path: &str) -> std::io::Result<PathBuf> {
    Ok(run_dir
        .join(kind)
        .join(artifact_relative_path(logged_path)?))
}

fn same_action_identity(pending: &StoredAction, completed: &StoredAction) -> bool {
    pending.started_at_ns == completed.started_at_ns
        && pending.path == completed.path
        && pending.op == completed.op
        && pending.rename_to == completed.rename_to
}

fn is_safe_run_id(run_id: &str) -> bool {
    if run_id.is_empty() {
        return false;
    }
    let mut components = Path::new(run_id).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Resolve `<run_id>` argument: literal id OR `latest` (read symlink).
pub fn resolve_run_id(target: &Path, run_id_arg: &str) -> Option<String> {
    if run_id_arg != "latest" {
        return is_safe_run_id(run_id_arg).then(|| run_id_arg.to_string());
    }
    let latest = doctor_root(target).join("latest");
    let resolved = fs::read_link(&latest).ok()?;
    resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|id| is_safe_run_id(id))
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
/// `strict` (default true) fails closed if any backup is missing or any
/// after_hash mismatch is detected (C3 fix — caller manually modified
/// the post-mutation file; we refuse to clobber).
///
/// Holds an exclusive advisory lock on `<run_dir>/undo.lock` for the
/// duration of the body (H5 fix — prevents two concurrent undos from
/// racing on the same run-id).
pub fn run_undo(
    target: &Path,
    run_id: &str,
    dry_run: bool,
    strict: bool,
) -> std::io::Result<UndoSummary> {
    if !is_safe_run_id(run_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid doctor run-id {run_id:?}"),
        ));
    }
    let run_dir = doctor_root(target).join("runs").join(run_id);
    let actions_path = run_dir.join("actions.jsonl");
    let backups_dir = run_dir.join("backups");

    if !actions_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("actions.jsonl not found at {}", actions_path.display()),
        ));
    }

    // H5 fix: per-run-id advisory lock. Only one undo may run on a given
    // run-id at a time. Released when the function returns (lock_file
    // drops); fs2's exclusive lock dies with the process for crash
    // recovery.
    use fs2::FileExt;
    let _lock_file = if dry_run {
        None
    } else {
        let lock_path = run_dir.join("undo.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        if lock_file.try_lock_exclusive().is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("another undo is in progress on run-id {run_id}"),
            ));
        }
        Some(lock_file)
    };

    let mut summary = UndoSummary {
        run_id: run_id.to_string(),
        ..Default::default()
    };

    let f = fs::File::open(&actions_path)?;
    let raw_lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();

    // G-Crash-Window fix: collapse each pending entry only when its matching
    // completed entry arrives. Preserve raw action order otherwise; elapsed
    // nanoseconds are useful backup keys, not globally unique log sequence ids.
    let mut actions = Vec::<StoredAction>::new();
    for line in &raw_lines {
        if line.trim().is_empty() {
            continue;
        }
        let action: StoredAction = match serde_json::from_str(line) {
            Ok(a) => a,
            Err(_) => continue, // skip malformed; reported below
        };
        if action.phase.as_deref() == Some("completed")
            && let Some(pos) = actions.iter().rposition(|candidate| {
                candidate.phase.as_deref() == Some("pending")
                    && same_action_identity(candidate, &action)
            })
        {
            actions[pos] = action;
        } else {
            actions.push(action);
        }
    }

    // Process in reverse raw mutation order (most recent mutation first).
    actions.reverse();

    for action in actions {
        // Detect crash-window: phase=pending without subsequent completed.
        if action.phase.as_deref() == Some("pending") {
            // Mutation crashed mid-flight. Backup exists at
            // `backups/seq_<started_at_ns>/<rel>`. Restore from it.
            // We do NOT validate after_hash because the mutation may not
            // have completed (or may have completed after the pending log
            // but before the completed log was flushed).
            let target_file = logged_target_path(target, &action.path)?;
            let backup_file = action_backup_file(&backups_dir, &action)?;
            if dry_run {
                eprintln!(
                    "[dry-run] crash-window recovery: would restore {} from backup",
                    target_file.display()
                );
                continue;
            }
            if backup_file.exists() {
                if let Some(parent) = target_file.parent() {
                    fs::create_dir_all(parent)?;
                }
                let backup_bytes = read_regular_file_no_follow(&backup_file)?;
                let restore_mode = action.before_mode.unwrap_or(0o644);
                if super::mutate::atomic_write_file(&target_file, &backup_bytes, restore_mode)
                    .is_ok()
                {
                    summary.actions_replayed += 1;
                } else {
                    summary
                        .failures
                        .push(format!("crash-window restore failed for {}", action.path));
                }
            } else if action.before_hash == EMPTY_FILE_SHA256 {
                // File didn't exist before mutation. If it now exists,
                // mutation probably succeeded — quarantine the post-state.
                if target_file.exists() {
                    let quarantine =
                        run_artifact_path(&run_dir, "quarantine_crash_window", &action.path)?;
                    if let Some(parent) = quarantine.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let _ = fs::rename(&target_file, &quarantine);
                }
            } else {
                summary
                    .failures
                    .push(format!("crash-window: backup missing for {}", action.path));
            }
            continue;
        }

        // Normal completed-line processing follows.
        if !action.ok {
            // The mutation failed; mutate() already attempted rollback. Skip.
            summary.actions_skipped += 1;
            continue;
        }

        let target_file = logged_target_path(target, &action.path)?;
        // Pass-4 fix: read the per-mutation backup at
        // `backups/seq_<started_at_ns>/<rel>`. If `started_at_ns == 0`
        // (legacy actions.jsonl from pre-pass-4 runs), fall back to the
        // old flat layout `backups/<rel>`.
        let backup_file = action_backup_file(&backups_dir, &action)?;

        match action.op.as_str() {
            "WriteFile" | "AppendFile" | "Chmod" => {
                if !backup_file.exists() {
                    if action.before_hash == EMPTY_FILE_SHA256 {
                        // before_hash of empty file: target didn't exist before
                        // the mutation. Undo by quarantining the new file.
                        //
                        // C3 fix: BEFORE quarantining, verify the live file
                        // still matches `after_hash`. If not, the user (or
                        // another process) modified the file post-mutation —
                        // refuse to clobber their changes (strict) or warn
                        // (non-strict).
                        let target_meta = fs::symlink_metadata(&target_file).ok();
                        if target_meta.is_none() {
                            // Already gone (user already deleted it, perhaps).
                            // Idempotent — count as no-op replay.
                            summary.actions_skipped += 1;
                            continue;
                        }
                        if !action.after_hash.is_empty() {
                            if let Some(m) = target_meta.as_ref()
                                && m.file_type().is_symlink()
                            {
                                let msg = format!(
                                    "target {} is a symlink; refusing to follow (G2 symlink-attack defense)",
                                    action.path
                                );
                                if strict {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::PermissionDenied,
                                        msg,
                                    ));
                                }
                                summary.failures.push(msg);
                                continue;
                            }
                            match read_regular_file_no_follow(&target_file) {
                                Ok(bytes) => {
                                    let cur_hash = sha256_hex(&bytes);
                                    if cur_hash != action.after_hash {
                                        let msg = format!(
                                            "would-quarantine target {} no longer matches mutation result (hash {} != recorded after_hash {}); refusing to clobber user-modified file",
                                            action.path, cur_hash, action.after_hash,
                                        );
                                        if strict {
                                            return Err(std::io::Error::new(
                                                std::io::ErrorKind::AlreadyExists,
                                                msg,
                                            ));
                                        }
                                        summary.failures.push(msg);
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    if strict {
                                        return Err(e);
                                    }
                                    summary.failures.push(format!(
                                        "could not re-read {} for after_hash check: {}",
                                        target_file.display(),
                                        e
                                    ));
                                    continue;
                                }
                            }
                        }
                        let quarantine =
                            run_artifact_path(&run_dir, "quarantine_undo", &action.path)?;
                        if dry_run {
                            eprintln!(
                                "[dry-run] would quarantine new file {}",
                                target_file.display()
                            );
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
                // Codex-C1 (round 2): verify the live file STILL matches
                // `after_hash` before restoring. If user modified the file
                // post-fix, undo refuses to clobber their work.
                if !action.after_hash.is_empty() {
                    // G2 (round 2): refuse to follow a symlink at the target.
                    // `fs::read` follows symlinks, which would let an attacker
                    // redirect undo to overwrite arbitrary files.
                    match fs::symlink_metadata(&target_file) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            let msg = format!(
                                "target {} is a symlink; refusing to follow (G2 symlink-attack defense)",
                                action.path
                            );
                            if strict {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::PermissionDenied,
                                    msg,
                                ));
                            }
                            summary.failures.push(msg);
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            let msg = format!(
                                "would-restore target {} is missing; refusing to resurrect a user-deleted post-fix file",
                                action.path
                            );
                            if strict {
                                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
                            }
                            summary.failures.push(msg);
                            continue;
                        }
                        Err(e) => {
                            if strict {
                                return Err(e);
                            }
                            summary.failures.push(format!(
                                "could not stat {} for after_hash check: {}",
                                target_file.display(),
                                e
                            ));
                            continue;
                        }
                    }
                    match read_regular_file_no_follow(&target_file) {
                        Ok(bytes) => {
                            let cur_hash = sha256_hex(&bytes);
                            if cur_hash != action.after_hash {
                                let msg = format!(
                                    "would-restore target {} no longer matches mutation result (hash {} != recorded after_hash {}); refusing to clobber user-modified file",
                                    action.path, cur_hash, action.after_hash,
                                );
                                if strict {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::AlreadyExists,
                                        msg,
                                    ));
                                }
                                summary.failures.push(msg);
                                continue;
                            }
                        }
                        Err(e) => {
                            if strict {
                                return Err(e);
                            }
                            summary.failures.push(format!(
                                "could not re-read {} for after_hash check: {}",
                                target_file.display(),
                                e
                            ));
                            continue;
                        }
                    }
                }
                if dry_run {
                    eprintln!(
                        "[dry-run] would restore {} from backup",
                        target_file.display()
                    );
                    summary.actions_replayed += 1;
                    continue;
                }
                if let Some(parent) = target_file.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Codex-C2 (round 2): atomic restore via tempfile. Was
                // non-atomic `fs::copy` which could leave a torn file on
                // disk-full / I/O fault. Now read backup bytes into memory,
                // then atomic-write through the chokepoint helper.
                let backup_bytes = match read_regular_file_no_follow(&backup_file) {
                    Ok(b) => b,
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary.failures.push(format!(
                            "could not read backup {}: {}",
                            backup_file.display(),
                            e
                        ));
                        continue;
                    }
                };
                let restore_mode = action.before_mode.unwrap_or(0o644);
                match super::mutate::atomic_write_file(&target_file, &backup_bytes, restore_mode) {
                    Ok(_) => {
                        // C2 fix: verify post-restore hash matches before_hash.
                        // If not, the backup is corrupt or tampered — refuse.
                        match read_regular_file_no_follow(&target_file) {
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
                let from_after = logged_target_path(target, rename_to)?;
                let restore_to = logged_target_path(target, &action.path)?;
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
                let current_meta = fs::symlink_metadata(&target_file);
                let current_exists = current_meta.is_ok();
                let current_is_symlink = current_meta
                    .as_ref()
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false);

                if !current_exists && backup_file.exists() && !action.after_hash.is_empty() {
                    let msg = format!(
                        "would-restore symlink {} is missing; refusing to resurrect a user-deleted post-fix link",
                        action.path,
                    );
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }

                if current_exists && !current_is_symlink {
                    let msg = format!(
                        "target {} is not a symlink; refusing to replace it during SymlinkAtomic undo",
                        action.path,
                    );
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }

                if current_is_symlink && !action.after_hash.is_empty() {
                    let cur_hash = symlink_target_hash(&target_file)?;
                    if cur_hash != action.after_hash {
                        let msg = format!(
                            "would-restore symlink {} no longer matches mutation result (hash {} != recorded after_hash {}); refusing to clobber user-modified link",
                            action.path, cur_hash, action.after_hash,
                        );
                        if strict {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::AlreadyExists,
                                msg,
                            ));
                        }
                        summary.failures.push(msg);
                        continue;
                    }
                }

                if !backup_file.exists() {
                    if action.before_hash == EMPTY_FILE_SHA256 {
                        if !current_exists {
                            summary.actions_skipped += 1;
                            continue;
                        }
                        let quarantine =
                            run_artifact_path(&run_dir, "quarantine_undo", &action.path)?;
                        if dry_run {
                            eprintln!(
                                "[dry-run] would quarantine new symlink {}",
                                target_file.display()
                            );
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
                                    "could not quarantine symlink {}: {}",
                                    target_file.display(),
                                    e
                                ));
                            }
                        }
                        continue;
                    }
                    let msg = format!("symlink backup missing for {}", action.path);
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }

                if dry_run {
                    eprintln!(
                        "[dry-run] would restore symlink {} from backup",
                        target_file.display()
                    );
                    summary.actions_replayed += 1;
                    continue;
                }

                if let Some(parent) = target_file.parent() {
                    fs::create_dir_all(parent)?;
                }
                let restore_target =
                    path_from_raw_bytes(read_regular_file_no_follow(&backup_file)?);
                match super::mutate::atomic_symlink(&target_file, &restore_target) {
                    Ok(()) => {
                        let restored_hash = symlink_target_hash(&target_file)?;
                        if restored_hash != action.before_hash {
                            let msg = format!(
                                "post-restore symlink hash mismatch for {}: expected {}, got {}",
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
                            "could not restore symlink {}: {}",
                            target_file.display(),
                            e
                        ));
                    }
                }
            }
            "DbExec" | "DbMigrate" => {
                // DB-row level undo requires the project's DbConn + a saved
                // .dump of the affected rows. Wired by the dispatch layer
                // in pass-2. For pass-1, mark as skipped (these ops aren't
                // emitted by any pass-1 fixer yet).
                summary.actions_skipped += 1;
            }
            other => {
                summary.failures.push(format!("unknown op kind: {}", other));
            }
        }
    }

    if !dry_run && summary.failures.is_empty() {
        // Codex-H1 (round 2): mark undo complete ONLY when no failures.
        // Was: always wrote the sentinel, then `undo_complete()` returned
        // true on retry → repo stranded in half-undone state with no
        // supported retry path. Now retry replays the missing actions.
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
    if !is_safe_run_id(run_id) {
        return false;
    }
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
    use crate::doctor::mutate::{Capabilities, MutateContext, Op, mutate};
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
            extra_locks: Vec::new(),
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
        assert!(
            !doctor_root(td.path())
                .join("runs")
                .join(run_id)
                .join("undo.lock")
                .exists(),
            "dry-run undo must not create lock artifacts"
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
        assert!(
            !quarantine.exists(),
            "quarantine should be empty after undo"
        );
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

    #[test]
    fn resolve_run_id_rejects_path_components() {
        let td = TempDir::new().unwrap();

        assert_eq!(resolve_run_id(td.path(), "../escape"), None);
        assert_eq!(resolve_run_id(td.path(), "nested/run"), None);
        assert_eq!(resolve_run_id(td.path(), "."), None);
        assert_eq!(resolve_run_id(td.path(), ""), None);
    }

    #[test]
    fn run_undo_rejects_path_component_run_id_before_path_join() {
        let td = TempDir::new().unwrap();
        let err = run_undo(td.path(), "../escape", false, true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_undo_rejects_parent_components_in_action_path() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-12T14-30-00Z__bad-action-path";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        fs::write(
            run_dir.join("actions.jsonl"),
            serde_json::json!({
                "path": "../escape.txt",
                "op": "WriteFile",
                "before_hash": EMPTY_FILE_SHA256,
                "after_hash": "",
                "started_at_ns": 0,
                "ok": true
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let err = run_undo(td.path(), run_id, false, true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_undo_rejects_parent_components_in_rename_destination() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-12T14-31-00Z__bad-rename-path";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        fs::write(
            run_dir.join("actions.jsonl"),
            serde_json::json!({
                "path": "original.txt",
                "op": "Rename",
                "before_hash": EMPTY_FILE_SHA256,
                "after_hash": "",
                "started_at_ns": 0,
                "rename_to": "../escape.txt",
                "ok": true
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let err = run_undo(td.path(), run_id, false, true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn undo_quarantines_symlink_created_by_symlink_atomic() {
        let td = TempDir::new().unwrap();
        let latest = td.path().join("latest");
        let new_target = PathBuf::from("runs/new");
        let run_id = "2026-05-12T12-30-00Z__symlink-new";
        let ctx = make_ctx(&td, run_id);

        mutate(
            &ctx,
            &latest,
            Op::SymlinkAtomic {
                target: new_target.clone(),
            },
        )
        .unwrap();
        drop(ctx);

        assert_eq!(fs::read_link(&latest).unwrap(), new_target);

        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert!(
            fs::symlink_metadata(&latest).is_err(),
            "created symlink should be moved to undo quarantine"
        );

        let quarantined = doctor_root(td.path())
            .join("runs")
            .join(run_id)
            .join("quarantine_undo")
            .join("latest");
        assert!(
            fs::symlink_metadata(&quarantined)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(quarantined).unwrap(),
            PathBuf::from("runs/new")
        );
    }

    #[test]
    fn undo_restores_previous_symlink_target_for_symlink_atomic() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("runs").join("old")).unwrap();
        fs::create_dir_all(td.path().join("runs").join("new")).unwrap();
        let latest = td.path().join("latest");
        std::os::unix::fs::symlink(Path::new("runs/old"), &latest).unwrap();

        let run_id = "2026-05-12T12-31-00Z__symlink-old";
        let ctx = make_ctx(&td, run_id);
        mutate(
            &ctx,
            &latest,
            Op::SymlinkAtomic {
                target: PathBuf::from("runs/new"),
            },
        )
        .unwrap();
        drop(ctx);

        assert_eq!(fs::read_link(&latest).unwrap(), PathBuf::from("runs/new"));

        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(fs::read_link(&latest).unwrap(), PathBuf::from("runs/old"));
    }

    #[test]
    fn undo_refuses_missing_post_fix_symlink_with_backup() {
        let td = TempDir::new().unwrap();
        let latest = td.path().join("latest");
        std::os::unix::fs::symlink(Path::new("runs/old"), &latest).unwrap();

        let run_id = "2026-05-12T13-00-00Z__symlink-missing";
        let ctx = make_ctx(&td, run_id);
        mutate(
            &ctx,
            &latest,
            Op::SymlinkAtomic {
                target: PathBuf::from("runs/new"),
            },
        )
        .unwrap();
        drop(ctx);

        std::fs::remove_file(&latest).unwrap();

        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            fs::symlink_metadata(&latest).is_err(),
            "strict undo must not recreate a user-deleted symlink"
        );
    }

    #[test]
    fn undo_c3_refuses_to_clobber_user_modified_post_fix_file() {
        // C3 fix: empty-before-hash branch verifies after_hash before
        // quarantining. If user modified the file post-fix, undo refuses.
        let td = TempDir::new().unwrap();
        let target = td.path().join("created_by_doctor.txt");
        // Pre-state: file does not exist (before_hash will be empty).
        let run_id = "2026-05-09T16-30-15Z__c3test";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"doctor wrote this\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        // User modifies the file post-fix.
        std::fs::write(&target, b"USER EDITED THIS\n").unwrap();
        // Undo in strict mode should refuse with AlreadyExists.
        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err(), "strict undo must refuse to clobber");
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // User's edit is preserved.
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "USER EDITED THIS\n",
            "user's post-fix edit must not be clobbered"
        );
    }

    #[test]
    fn undo_c3_non_strict_records_failure_but_preserves_file() {
        // Same scenario as above but non-strict — undo records the failure
        // and preserves the user's edit.
        let td = TempDir::new().unwrap();
        let target = td.path().join("created_by_doctor.txt");
        let run_id = "2026-05-09T16-30-15Z__c3laxtest";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"doctor wrote this\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        std::fs::write(&target, b"USER EDITED THIS\n").unwrap();
        let summary = run_undo(td.path(), run_id, false, false).unwrap();
        assert_eq!(summary.actions_replayed, 0);
        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0].contains("user-modified"),
            "got: {}",
            summary.failures[0]
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "USER EDITED THIS\n",
            "user's post-fix edit must not be clobbered (non-strict)"
        );
    }

    #[test]
    fn undo_codex_c1_refuses_user_modified_post_fix_writefile() {
        // Codex round-2 C1: in the WriteFile branch (non-empty backup),
        // undo verifies after_hash before clobbering user edits.
        let td = TempDir::new().unwrap();
        let target = td.path().join("config.toml");
        std::fs::write(&target, b"# original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__codex-c1";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"# doctor wrote\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        // User edits the file post-fix.
        std::fs::write(&target, b"# user edited\n").unwrap();
        // Undo refuses (strict) — does not clobber user edits.
        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // User's edit preserved.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# user edited\n");
    }

    #[test]
    fn undo_codex_c1_refuses_missing_post_fix_writefile() {
        // A missing target is also a post-fix modification. Strict undo must
        // not silently resurrect a file the user removed after the doctor run.
        let td = TempDir::new().unwrap();
        let target = td.path().join("config.toml");
        std::fs::write(&target, b"# original\n").unwrap();
        let run_id = "2026-05-12T13-05-00Z__codex-c1-missing";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"# doctor wrote\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);

        std::fs::remove_file(&target).unwrap();

        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            fs::symlink_metadata(&target).is_err(),
            "strict undo must not recreate a user-deleted target"
        );
    }

    #[test]
    fn undo_codex_h1_no_sentinel_on_failure() {
        // Codex round-2 H1: undo_complete sentinel is only written when
        // there are no failures. Was: always written, stranding the repo
        // in half-undone state on retry.
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        std::fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__codex-h1";
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
        // User modifies post-fix → undo refuses (in non-strict, records
        // failure but continues).
        std::fs::write(&target, b"USER\n").unwrap();
        let summary = run_undo(td.path(), run_id, false, false).unwrap();
        assert_eq!(summary.failures.len(), 1);
        // Sentinel must NOT be present — repo is half-undone, retry should
        // be supported.
        assert!(
            !undo_complete(td.path(), run_id),
            "sentinel must not be written when failures present (Codex-H1)"
        );
    }

    #[test]
    fn undo_g2_refuses_symlink_target() {
        // G2 (round 2): undo refuses to follow a symlink at the target.
        // Symlink-attack defense — without this, a hostile symlink would
        // let undo overwrite arbitrary files outside the safety envelope.
        let td = TempDir::new().unwrap();
        let target = td.path().join("config.toml");
        std::fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__g2";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"doctor wrote\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        // Replace target with a symlink pointing at a "sensitive" file.
        std::fs::remove_file(&target).unwrap();
        let sensitive = td.path().join("sensitive_secret.txt");
        std::fs::write(&sensitive, b"secret data\n").unwrap();
        std::os::unix::fs::symlink(&sensitive, &target).unwrap();
        // Undo must refuse to follow the symlink.
        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        // Sensitive file untouched.
        assert_eq!(
            std::fs::read_to_string(&sensitive).unwrap(),
            "secret data\n"
        );
    }

    #[test]
    fn undo_refuses_symlink_backup_artifact() {
        // Backups live under the doctor run directory, but they are still
        // filesystem inputs. Undo must restore only from regular backup files.
        let td = TempDir::new().unwrap();
        let target = td.path().join("config.toml");
        std::fs::write(&target, b"doctor wrote\n").unwrap();
        let run_id = "2026-05-12T14-00-00Z__backup-symlink";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();

        let backup = run_dir.join("backups").join("config.toml");
        std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
        let sensitive = td.path().join("sensitive_backup_source.txt");
        std::fs::write(&sensitive, b"original\n").unwrap();
        std::os::unix::fs::symlink(&sensitive, &backup).unwrap();

        let mut actions = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        writeln!(
            actions,
            "{}",
            serde_json::json!({
                "path": "config.toml",
                "op": "WriteFile",
                "before_hash": sha256_hex(b"original\n"),
                "after_hash": sha256_hex(b"doctor wrote\n"),
                "before_mode": 0o644,
                "ok": true,
            })
        )
        .unwrap();
        drop(actions);

        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "doctor wrote\n",
            "target must remain in its post-fix state"
        );
        assert_eq!(
            std::fs::read_to_string(&sensitive).unwrap(),
            "original\n",
            "symlink target must not be consumed as a backup"
        );
    }

    #[test]
    fn undo_g2_refuses_symlink_target_for_created_file_quarantine_branch() {
        // Same defense as `undo_g2_refuses_symlink_target`, but for files
        // that did not exist before the doctor mutation. This branch verifies
        // after_hash before quarantining the created file and must not follow
        // attacker-controlled symlinks while doing that verification.
        let td = TempDir::new().unwrap();
        let target = td.path().join("created_by_doctor.txt");
        let run_id = "2026-05-12T10-05-00Z__g2-created";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"doctor wrote this\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);

        std::fs::remove_file(&target).unwrap();
        let sensitive = td.path().join("sensitive_secret.txt");
        std::fs::write(&sensitive, b"doctor wrote this\n").unwrap();
        std::os::unix::fs::symlink(&sensitive, &target).unwrap();

        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink leaf should remain untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&sensitive).unwrap(),
            "doctor wrote this\n"
        );
    }

    #[test]
    fn undo_g2_refuses_dangling_symlink_for_created_file_quarantine_branch() {
        // `Path::exists()` is false for dangling symlinks. The created-file
        // branch must use symlink_metadata so it does not skip a hostile link.
        let td = TempDir::new().unwrap();
        let target = td.path().join("created_by_doctor.txt");
        let run_id = "2026-05-12T13-10-00Z__g2-dangling";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"doctor wrote this\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);

        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(td.path().join("missing-target"), &target).unwrap();

        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "dangling symlink leaf should remain untouched"
        );
    }

    #[test]
    fn pass5_g_crash_window_recovery() {
        // Pass-5 G-Crash-Window: simulate a crash mid-mutation by writing
        // ONLY the pending line (no completed line). Run undo: must
        // recognize the crash-window and restore from backup.
        let td = TempDir::new().unwrap();
        let target = td.path().join("crash.txt");
        std::fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-10T07-00-00Z__crashwindow";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();

        // Manually write the backup (as if mutate's step 5 had completed).
        let started_at_ns: u128 = 12_345_000_000;
        let backup_dir = run_dir
            .join("backups")
            .join(format!("seq_{:026}", started_at_ns));
        fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("crash.txt"), b"original\n").unwrap();

        // Manually write a "pending" actions.jsonl line (as if mutate had
        // recorded its intent before crashing).
        let pending = serde_json::json!({
            "path": "crash.txt",
            "op": "WriteFile",
            "before_hash": format!("sha256:{:x}", Sha256::digest(b"original\n")),
            "after_hash": "",
            "started_at_ns": started_at_ns,
            "finished_at_ns": 0,
            "run_id": run_id,
            "fixer_id": "test-fixer",
            "ok": false,
            "phase": "pending",
            "before_mode": 0o644,
        });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        f.write_all(serde_json::to_string(&pending).unwrap().as_bytes())
            .unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);

        // Simulate the post-crash state: file was mutated but completion
        // never logged.
        std::fs::write(&target, b"halfway through mutation\n").unwrap();

        // Run undo. It should detect the crash-window and restore.
        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(
            summary.actions_replayed, 1,
            "crash-window recovery should count as 1 replay"
        );
        // File restored byte-identical to original.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original\n",
            "crash-window restore must produce byte-identical original"
        );
    }

    #[test]
    fn pass5_g_crash_window_dry_run_does_not_restore() {
        // Same setup as above, but dry_run=true. Must NOT restore.
        let td = TempDir::new().unwrap();
        let target = td.path().join("crash.txt");
        std::fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-10T07-00-00Z__crashdryrun";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();

        let started_at_ns: u128 = 99_999_000_000;
        let backup_dir = run_dir
            .join("backups")
            .join(format!("seq_{:026}", started_at_ns));
        fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("crash.txt"), b"original\n").unwrap();

        let pending = serde_json::json!({
            "path": "crash.txt",
            "op": "WriteFile",
            "before_hash": format!("sha256:{:x}", Sha256::digest(b"original\n")),
            "after_hash": "",
            "started_at_ns": started_at_ns,
            "finished_at_ns": 0,
            "run_id": run_id,
            "fixer_id": "test-fixer",
            "ok": false,
            "phase": "pending",
        });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        f.write_all(serde_json::to_string(&pending).unwrap().as_bytes())
            .unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);

        std::fs::write(&target, b"halfway through mutation\n").unwrap();

        let _ = run_undo(td.path(), run_id, true, true).unwrap();
        // Dry-run must not write.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "halfway through mutation\n",
            "dry-run must not restore"
        );
    }

    #[test]
    fn pass5_completed_line_supersedes_pending_line() {
        // Pass-5: when both pending and completed exist for the same
        // started_at_ns, the completed line wins (standard undo path,
        // not crash-window).
        let td = TempDir::new().unwrap();
        let target = td.path().join("normal.txt");
        std::fs::write(&target, b"v0").unwrap();
        let run_id = "2026-05-10T07-00-00Z__normalundo";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"v1".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        // Verify both pending + completed exist in actions.jsonl.
        let actions = std::fs::read_to_string(
            td.path()
                .join(".doctor")
                .join("runs")
                .join(run_id)
                .join("actions.jsonl"),
        )
        .unwrap();
        assert_eq!(actions.lines().count(), 2);
        // Run undo. Should restore via the standard completed-line path.
        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0");
    }

    #[test]
    fn undo_preserves_distinct_actions_with_same_started_at_ns() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-10T07-00-00Z__sameclock";
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let started_at_ns: u128 = 7_777_000_000;

        let alpha = td.path().join("alpha.txt");
        let beta = td.path().join("beta.txt");
        std::fs::write(&alpha, b"alpha new\n").unwrap();
        std::fs::write(&beta, b"beta new\n").unwrap();

        let backup_dir = run_dir
            .join("backups")
            .join(format!("seq_{:026}", started_at_ns));
        fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("alpha.txt"), b"alpha original\n").unwrap();
        std::fs::write(backup_dir.join("beta.txt"), b"beta original\n").unwrap();

        let action_pair = |path: &str, before: &[u8], after: &[u8]| {
            let pending = serde_json::json!({
                "path": path,
                "op": "WriteFile",
                "before_hash": sha256_hex(before),
                "after_hash": "",
                "started_at_ns": started_at_ns,
                "finished_at_ns": 0,
                "run_id": run_id,
                "fixer_id": "test-fixer",
                "ok": false,
                "phase": "pending",
                "before_mode": 0o644,
            });
            let completed = serde_json::json!({
                "path": path,
                "op": "WriteFile",
                "before_hash": sha256_hex(before),
                "after_hash": sha256_hex(after),
                "started_at_ns": started_at_ns,
                "finished_at_ns": started_at_ns + 1,
                "run_id": run_id,
                "fixer_id": "test-fixer",
                "ok": true,
                "phase": "completed",
                "before_mode": 0o644,
            });
            [pending, completed]
        };

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        for action in action_pair("alpha.txt", b"alpha original\n", b"alpha new\n")
            .into_iter()
            .chain(action_pair("beta.txt", b"beta original\n", b"beta new\n"))
        {
            f.write_all(serde_json::to_string(&action).unwrap().as_bytes())
                .unwrap();
            f.write_all(b"\n").unwrap();
        }
        drop(f);

        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 2);
        assert_eq!(std::fs::read_to_string(&alpha).unwrap(), "alpha original\n");
        assert_eq!(std::fs::read_to_string(&beta).unwrap(), "beta original\n");
    }

    #[test]
    fn undo_restores_absolute_path_backups_outside_repo_root() {
        let repo = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("outside.txt");
        std::fs::write(&target, b"outside original\n").unwrap();

        let run_id = "2026-05-10T07-00-00Z__absolute";
        let run_dir = scaffold_run_dir(repo.path(), run_id).unwrap();
        let actions = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
            run_id: run_id.to_string(),
            run_dir,
            capabilities: Capabilities {
                write_scopes: vec![outside.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: "test-fixer".into(),
            repo_root: repo.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        };

        mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"outside new\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside new\n");

        let summary = run_undo(repo.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "outside original\n"
        );
    }

    #[test]
    fn undo_h5_concurrent_undo_blocks() {
        // H5 fix: per-run-id flock prevents concurrent undo invocations.
        // Acquire the lock manually first; the run_undo call should refuse.
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        std::fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-09T16-30-15Z__h5test";
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
        // Manually acquire the lock as a "competing" process.
        let run_dir = doctor_root(td.path()).join("runs").join(run_id);
        fs::create_dir_all(&run_dir).unwrap();
        let lock_path = run_dir.join("undo.lock");
        let competitor = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        competitor.try_lock_exclusive().unwrap();
        // Now run_undo should refuse with WouldBlock.
        let result = run_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        // Release the competitor's lock.
        FileExt::unlock(&competitor).unwrap();
        // Now run_undo should succeed.
        let summary = run_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
    }
}
