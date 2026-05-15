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
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
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

/// Read a regular file into memory with O_NOFOLLOW + post-open
/// metadata check.
///
/// Round-6 (Gemini F2): the pre-round-6 version used
/// `fs::symlink_metadata` followed by `read_regular_file_no_follow_inner`,
/// which only rejected symlinks via O_NOFOLLOW. An attacker could
/// swap the target to a character device (e.g., `/dev/zero`) or a
/// FIFO between the metadata check and the open, and `read_to_end`
/// would consume memory until OOM (or block indefinitely). The fix
/// routes through `open_regular_file_no_follow`, which verifies
/// `file_type().is_file()` on the **held fd** — closes the race
/// window because metadata-on-fd refers to the same inode the read
/// will consume from.
fn read_regular_file_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = open_regular_file_no_follow(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Open `path` for reading with `O_NOFOLLOW | O_NONBLOCK`; verify
/// the opened fd is a regular file; return the File.
///
/// Round-5 introduced the helper; round-6 (Gemini F4) added
/// `O_NONBLOCK` so a FIFO-swap can't DoS the open by blocking on
/// a missing writer. `O_NONBLOCK` is a no-op for regular files
/// (which is what the post-open `is_file()` check enforces), so
/// this never changes behavior on the legitimate paths.
///
/// Round-6 (Gemini F2 / Codex F1) also makes this the single
/// gateway for both byte-reads and streaming hashes — the
/// post-open `metadata().file_type().is_file()` check defeats the
/// classic `symlink_metadata → open` TOCTOU.
#[cfg(unix)]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let f = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(f)
}

#[cfg(not(unix))]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let f = OpenOptions::new().read(true).open(path)?;
    let meta = f.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(f)
}

/// Stream-hash a regular file with O_NOFOLLOW. Memory is O(1) (64KB
/// chunk buffer), so multi-GB SQLite DBs hash without OOM.
///
/// Round-5 (Codex F3): the round-4 verifier used `read_regular_file_no_follow`
/// which materialized the entire DB into a `Vec<u8>`. For multi-GB mailbox
/// DBs that is a recovery-time memory bomb. This helper streams 64KB chunks
/// directly into Sha256 (mirrors `mutate::sha256_of_path`).
fn sha256_stream_no_follow(path: &Path) -> std::io::Result<String> {
    let mut f = open_regular_file_no_follow(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Round-5/round-6 — atomic streaming restore of a SQLite DB (or
/// any large regular file) from a backup, with a per-chunk hash
/// computed in the same pass and a final pre-rename re-check.
///
/// 1. Refuse if `target_file` exists and is a symlink (G2 defense
///    — match the existing WriteFile/Chmod paths). `rename(2)` over
///    a symlink at `target_file` does not follow it, but `fs::copy`
///    did — this guard blocks the symlink-overwrite path entirely.
/// 2. Stream-copy `backup_file` → a tempfile in `target_file`'s
///    parent dir while updating a `Sha256` hasher with each chunk.
/// 3. Verify the streamed hash equals `expected_hash`. If not,
///    drop the tempfile (tempfile crate auto-cleans on drop) and
///    return an error — the live `target_file` is **untouched**,
///    so a torn DB never appears on disk.
/// 4. fsync the tempfile data, set its permissions via fd.
/// 5. Round-6 (Gemini F3): if `expected_target_after_hash` is
///    `Some(...)`, re-stream-hash the live `target_file` and refuse
///    the rename if it no longer matches. This closes the
///    seconds-long race window between the outer user-edit defense
///    and the rename, during which the operator could have
///    modified the live DB while the streaming copy ran.
/// 6. persist tmp over `target_file` via `rename(2)` (atomic, does
///    not follow symlink at destination on Unix).
/// 7. Best-effort parent-directory fsync. Round-6 Codex F2
///    asked to propagate the error; round-7 Gemini F1 showed
///    propagating an fsync error AFTER a successful rename
///    strands undo mid-replay (live target matches before_hash
///    on retry, user-edit defense refuses). Reverted to
///    best-effort `let _ = ...` matching
///    `mutate::atomic_write_file`'s pattern. Durability is "as
///    durable as `rename(2)`" — sufficient for ext4/xfs crash
///    recovery on Linux.
///
/// Note: `backup_file` is opened with O_NOFOLLOW + O_NONBLOCK
/// (round-5 + round-6) — symlinks at the backup are refused
/// and the FIFO-swap DoS is defeated.
fn atomic_restore_db(
    backup_file: &Path,
    target_file: &Path,
    expected_hash: &str,
    expected_target_after_hash: Option<&str>,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // 1. Refuse symlink at target. `rename(2)` is symlink-safe at dst, but
    // the symmetric existing G2 defense documents intent and gives a
    // useful error message before any disk I/O. Round-6 (Codex F1 P2):
    // fail closed if metadata fails for any reason other than ENOENT —
    // EACCES/ELOOP/etc. must not silently downgrade to "target absent".
    match fs::symlink_metadata(target_file) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "target {} is a symlink; refusing to follow (round-5 G2 defense for DB restore)",
                    target_file.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    // 2. Parent dir for tempfile + final rename.
    let parent = target_file.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    // 3. Stream-copy with parallel hashing.
    let mut src = open_regular_file_no_follow(backup_file)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut hasher = Sha256::new();
    {
        let mut dst = tmp.as_file();
        let mut buf = vec![0u8; 65_536];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
        }
        dst.sync_data()?;
        #[cfg(unix)]
        dst.set_permissions(std::fs::Permissions::from_mode(mode))?;
        #[cfg(not(unix))]
        let _ = mode;
    }

    // 4. Verify hash BEFORE swapping over the live target. On mismatch,
    // the tempfile drops here (auto-cleaned), the live target is left
    // intact, and the caller records the failure.
    let restored_hash = format!("sha256:{}", hex::encode(hasher.finalize()));
    if restored_hash != expected_hash {
        return Err(std::io::Error::other(format!(
            "post-stream DB hash mismatch: expected {expected_hash}, got {restored_hash}",
        )));
    }

    // 5. Round-6 (Gemini F3 P1): close the multi-second race between
    // the outer user-edit defense (which streamed and hashed the
    // target before we entered) and the rename. Re-stream-hash the
    // live target and refuse if it no longer matches `after_hash`.
    // We skip this if the target doesn't exist (target-was-removed
    // case from round-4) or if no expectation was supplied
    // (DbMigrate marker op or pre-pass-34 records).
    if let Some(expected_after) = expected_target_after_hash
        && fs::symlink_metadata(target_file).is_ok()
    {
        match sha256_stream_no_follow(target_file) {
            Ok(cur_hash) => {
                if cur_hash != expected_after {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            "live target {} drifted during restore (hash {} != recorded after_hash {}); refusing to clobber",
                            target_file.display(),
                            cur_hash,
                            expected_after,
                        ),
                    ));
                }
            }
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "could not re-hash live target {} for pre-rename re-check: {e}",
                    target_file.display(),
                )));
            }
        }
    }

    // 6. Atomic rename. `rename(2)` does not follow a symlink at
    // the destination.
    tmp.persist(target_file).map_err(|e| e.error)?;

    // 7. Best-effort parent-dir fsync. Round-6 Codex F2 asked to
    // propagate; round-7 Gemini F1 pointed out that propagating
    // an fsync error AFTER a successful rename strands undo
    // mid-replay: the on-disk state is restored, but undo bubbles
    // the error and refuses to record the completion sentinel.
    // The retry then sees the live target match `before_hash`
    // (not `after_hash`) and the user-edit defense refuses,
    // leaving the run permanently unrecoverable. We match the
    // chokepoint's own `mutate::atomic_write_file` pattern:
    // best-effort fsync, swallow the result. Durability is "as
    // durable as the rename" — which is what fs::rename promises
    // on Linux ext4/xfs even without an explicit parent fsync
    // for the purpose of crash recovery.
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|d| d.sync_all());
    Ok(())
}

/// Recreate `dir` (and ancestors) with `0o700` on Unix. Round-5
/// Gemini F6: the default `fs::create_dir_all` applies `0o777 &
/// !umask`, which on a default `umask=022` gives `0o755`. That
/// strips the security envelope of sensitive DB-storage dirs (the
/// canonical `~/.mcp_agent_mail_git_mailbox_repo` lives at
/// `0o700`). We err on the side of restrictive: if the operator
/// truly wants a wider mode they can chmod after recovery.
#[cfg(unix)]
fn ensure_parent_dir_strict(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn ensure_parent_dir_strict(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

/// Acquire the per-path `.<basename>.doctor-lock` for `target_file`.
/// Round-10 (round-7 Gemini F6 / Codex F2): mirrors the chokepoint's
/// own per-target advisory lock so concurrent `am doctor --fix` and
/// `am doctor undo` are properly serialized on the same path.
/// Without this, mutate could be hashing/writing the file while
/// undo is hashing/restoring it, producing torn intermediate state.
///
/// Returns:
/// - `Ok(Some(file))` — lock held; release on drop at scope exit.
/// - `Ok(None)` — parent dir doesn't exist (no concurrent mutation
///   possible against a non-existent dir; skip locking).
/// - `Err(WouldBlock)` — another process holds the lock.
/// - `Err(other)` — IO error opening or creating the lock file.
fn acquire_target_lock(target_file: &Path) -> std::io::Result<Option<std::fs::File>> {
    use fs2::FileExt;
    let parent = target_file.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        return Ok(None);
    }
    let basename = target_file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "_root_".to_string());
    let lock_path = parent.join(format!(".{basename}.doctor-lock"));
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    if let Err(error) = lock_file.try_lock_exclusive() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "target {} is locked by another doctor process (concurrent fix?)",
                    target_file.display()
                ),
            ));
        }
        return Err(error);
    }
    Ok(Some(lock_file))
}

/// Acquire `.doctor-lock` files on TWO paths in a stable order
/// (lexicographic) to prevent deadlock when concurrent operations
/// touch the same pair (e.g., Rename + reverse-Rename undo).
/// Returns both locks even if one of the inputs is the same path
/// (in which case the second call hits WouldBlock since fs2 advisory
/// locks are per-fd; we collapse same-path to a single lock).
fn acquire_pair_locks(
    a: &Path,
    b: &Path,
) -> std::io::Result<(Option<std::fs::File>, Option<std::fs::File>)> {
    if a == b {
        let lock = acquire_target_lock(a)?;
        return Ok((lock, None));
    }
    // Lexicographic order on the raw OsStr bytes is stable and
    // collision-free across concurrent processes.
    let (first_path, second_path) = if a.as_os_str() <= b.as_os_str() {
        (a, b)
    } else {
        (b, a)
    };
    let first_lock = acquire_target_lock(first_path)?;
    let second_lock = acquire_target_lock(second_path)?;
    // Re-map back to (a, b) order so the caller can reason about
    // which lock corresponds to which input path. The locks
    // themselves are interchangeable for the purposes of holding
    // exclusion until scope-drop.
    if a.as_os_str() <= b.as_os_str() {
        Ok((first_lock, second_lock))
    } else {
        Ok((second_lock, first_lock))
    }
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

/// Round-6 Gemini F1 (P0): enforce the same `write_scopes` trust
/// boundary that `mutate()` enforces at fix time. `actions.jsonl`
/// is filesystem state — if an attacker can plant `.doctor/runs/`
/// in the victim's repo (e.g., via a PR, a compromised dependency,
/// or already-write-access social engineering), an unauthenticated
/// `actions.jsonl` entry pointing at `/etc/passwd` or
/// `~/.ssh/authorized_keys` would otherwise let `am doctor undo`
/// silently overwrite system files with attacker-supplied bytes.
/// The fix re-applies `ensure_in_scope` at undo time against the
/// same defaults the runtime used at fix time.
fn enforce_scope(write_scopes: &[PathBuf], path: &Path) -> std::io::Result<()> {
    let caps = super::mutate::Capabilities {
        write_scopes: write_scopes.to_vec(),
    };
    super::mutate::ensure_in_scope(&caps, path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to restore {}: outside doctor write_scopes ({e})",
                path.display(),
            ),
        )
    })
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
/// Uses `default_write_scopes()` as the trust boundary. Round-6
/// (Gemini F1 P0) — a malicious `actions.jsonl` could otherwise
/// instruct undo to overwrite arbitrary files (e.g., `/etc/passwd`,
/// `~/.ssh/authorized_keys`) because `logged_target_path` honors
/// absolute paths verbatim. `mutate()` enforces scope at fix time,
/// but the round-5 undo path did not, leaving a path-traversal /
/// privilege-escalation vector for any attacker who could plant
/// (or commit) `.doctor/runs/<id>/` files in a victim's repo.
///
/// Tests that need a custom scope (the per-fixer test harnesses
/// pass `td.path()`) should call `run_undo_with_scopes`.
pub fn run_undo(
    target: &Path,
    run_id: &str,
    dry_run: bool,
    strict: bool,
) -> std::io::Result<UndoSummary> {
    let scopes = super::default_write_scopes();
    run_undo_with_scopes(target, run_id, dry_run, strict, &scopes)
}

/// Replay `actions.jsonl` in reverse, enforcing `write_scopes` as
/// the trust boundary. See `run_undo` for the contract.
///
/// `dry_run` prints what would happen without writing.
/// `strict` (default true) fails closed if any backup is missing or any
/// after_hash mismatch is detected (C3 fix — caller manually modified
/// the post-mutation file; we refuse to clobber).
///
/// Holds an exclusive advisory lock on `<run_dir>/undo.lock` for the
/// duration of the body (H5 fix — prevents two concurrent undos from
/// racing on the same run-id).
pub fn run_undo_with_scopes(
    target: &Path,
    run_id: &str,
    dry_run: bool,
    strict: bool,
    write_scopes: &[PathBuf],
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
            // Round-6 Gemini F1 (P0): scope check.
            if let Err(e) = enforce_scope(write_scopes, &target_file) {
                if strict {
                    return Err(e);
                }
                summary.failures.push(format!("{e}"));
                continue;
            }
            let backup_file = action_backup_file(&backups_dir, &action)?;
            if dry_run {
                eprintln!(
                    "[dry-run] crash-window recovery: would restore {} from backup",
                    target_file.display()
                );
                continue;
            }
            // Round-10: per-path lock acquisition. The chokepoint
            // takes `.<basename>.doctor-lock` exclusive for every
            // mutation; undo must match so concurrent fix+undo
            // can't tear the file between hash/write.
            let _crash_target_lock: Option<std::fs::File> = match acquire_target_lock(&target_file)
            {
                Ok(g) => g,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!(
                        "crash-window restore: {} held by concurrent fix; skipping",
                        target_file.display()
                    ));
                    continue;
                }
                Err(e) => {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!(
                        "could not acquire crash-window lock on {}: {}",
                        target_file.display(),
                        e,
                    ));
                    continue;
                }
            };
            if backup_file.exists() {
                // Round-8 Gemini F1 (P2): DbExec/DbMigrate
                // crash-window parent-dir recreation must use
                // the tight 0o700 mode, matching the completed-
                // branch behavior (round-5 Gemini F6). Other ops
                // keep `fs::create_dir_all` (default umask).
                let is_db_op = matches!(action.op.as_str(), "DbExec" | "DbMigrate");
                if let Some(parent) = target_file.parent() {
                    if is_db_op {
                        ensure_parent_dir_strict(parent)?;
                    } else {
                        fs::create_dir_all(parent)?;
                    }
                }
                // Round-7 Gemini F5 (P2): DbExec/DbMigrate crash-
                // window restore routes through the streaming
                // `atomic_restore_db` to avoid materializing
                // multi-GB SQLite DBs into memory. Other ops keep
                // the in-memory atomic_write_file path because
                // their backups are bounded by config/source-file
                // size. before_hash is empty for crash-window
                // records (post-state may not match anything), so
                // we skip the post-rename re-check by passing
                // None.
                //
                // Round-8 Gemini F4 (P3): DbExec/DbMigrate
                // default mode tightens to 0o600 (sensitive DB
                // storage), matching the completed branch. Other
                // ops keep 0o644.
                let restore_mode = if is_db_op {
                    action.before_mode.unwrap_or(0o600)
                } else {
                    action.before_mode.unwrap_or(0o644)
                };
                let restore_result = if is_db_op {
                    atomic_restore_db(
                        &backup_file,
                        &target_file,
                        &action.before_hash,
                        None,
                        restore_mode,
                    )
                } else {
                    // Round-8 Gemini F3 (P2): post-restore
                    // hash verification was missing for non-DB
                    // crash-window restores. Mirror the
                    // completed-branch Codex-C2 defense.
                    match read_regular_file_no_follow(&backup_file) {
                        Ok(bytes) => {
                            let write_res = super::mutate::atomic_write_file(
                                &target_file,
                                &bytes,
                                restore_mode,
                            );
                            if write_res.is_err() {
                                write_res
                            // Round-9 (Gemini F2 P3 clarification):
                            // `.is_empty()` only matches the
                            // literal empty string `""` from
                            // legacy / partial-flush actions.jsonl.
                            // The 0-byte file case uses the
                            // 71-char `EMPTY_FILE_SHA256` sentinel,
                            // so `.is_empty()` is `false` and the
                            // hash check correctly fires.
                            } else if !action.before_hash.is_empty() {
                                match read_regular_file_no_follow(&target_file) {
                                    Ok(restored) => {
                                        let restored_hash = sha256_hex(&restored);
                                        if restored_hash != action.before_hash {
                                            Err(std::io::Error::other(format!(
                                                "crash-window post-restore hash mismatch: expected {}, got {}",
                                                action.before_hash, restored_hash,
                                            )))
                                        } else {
                                            Ok(())
                                        }
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                Ok(())
                            }
                        }
                        Err(e) => Err(e),
                    }
                };
                if restore_result.is_ok() {
                    summary.actions_replayed += 1;
                } else {
                    summary.failures.push(format!(
                        "crash-window restore failed for {}: {}",
                        action.path,
                        restore_result
                            .err()
                            .map_or_else(String::new, |e| e.to_string()),
                    ));
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
        // Round-6 Gemini F1 (P0): scope check.
        if let Err(e) = enforce_scope(write_scopes, &target_file) {
            if strict {
                return Err(e);
            }
            summary.failures.push(format!("{e}"));
            continue;
        }
        // Round-10 (round-7 Codex F2 / Gemini F6): acquire the
        // per-path `.<basename>.doctor-lock` so concurrent
        // `am doctor --fix` and `am doctor undo` cannot interleave
        // a hash/restore step on the same target. For `Rename`
        // both sides need locking — handled inside the Rename arm
        // via `acquire_pair_locks`; the target_file lock here is
        // redundant in that case but harmless (mutate.rs also
        // takes the source lock first for renames, so the pair is
        // already serialized by the chokepoint).
        //
        // Dry-run skips the lock entirely — no mutation, no race.
        let _target_lock: Option<std::fs::File> = if dry_run || action.op == "Rename" {
            None
        } else {
            match acquire_target_lock(&target_file) {
                Ok(g) => g,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!(
                        "target {} held by concurrent fix; skipping",
                        target_file.display()
                    ));
                    continue;
                }
                Err(e) => {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!(
                        "could not acquire lock on {}: {}",
                        target_file.display(),
                        e,
                    ));
                    continue;
                }
            }
        };
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
                        //
                        // Round-7 Gemini F3 (P2): explicit match — fail
                        // closed on non-ENOENT errors (EACCES, ELOOP, etc.)
                        // rather than silently treating them as "target
                        // gone". Matches the DbExec branch's round-6 fix.
                        let target_meta = match fs::symlink_metadata(&target_file) {
                            Ok(m) => Some(m),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(e) => {
                                if strict {
                                    return Err(e);
                                }
                                summary.failures.push(format!(
                                    "could not stat {} for quarantine check: {}",
                                    target_file.display(),
                                    e,
                                ));
                                continue;
                            }
                        };
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
                // Round-6 Gemini F1 (P0): scope check on BOTH ends
                // of the rename. `action.path` was already checked
                // via target_file above; `rename_to` is the
                // additional surface and must also be in scope.
                if let Err(e) = enforce_scope(write_scopes, &from_after) {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!("{e}"));
                    continue;
                }
                if let Err(e) = enforce_scope(write_scopes, &restore_to) {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!("{e}"));
                    continue;
                }
                // Round-10: pair-lock acquisition. Both ends of
                // the rename need `.doctor-lock` so a concurrent
                // mutate can't race against us on either path.
                // `acquire_pair_locks` enforces stable
                // lexicographic order to prevent deadlock. Dry-run
                // skips since no mutation happens.
                let _rename_locks: (Option<std::fs::File>, Option<std::fs::File>) = if dry_run {
                    (None, None)
                } else {
                    match acquire_pair_locks(&from_after, &restore_to) {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if strict {
                                return Err(e);
                            }
                            summary.failures.push(format!(
                                "rename pair ({} ↔ {}) held by concurrent fix; skipping",
                                from_after.display(),
                                restore_to.display()
                            ));
                            continue;
                        }
                        Err(e) => {
                            if strict {
                                return Err(e);
                            }
                            summary
                                .failures
                                .push(format!("could not acquire rename pair locks: {}", e));
                            continue;
                        }
                    }
                };
                // Round-9 (Gemini F1 P3): the dry-run check was
                // previously here, BEFORE the after_hash and
                // destination-clobber checks. That made
                // `am doctor undo --dry-run` falsely report
                // "would succeed" for renames that a real run
                // would rightly refuse. Moved below all
                // non-mutating safety checks so dry-run mirrors
                // the actual decision.
                //
                // Round-8 (Codex F1 P2): defer parent-dir creation
                // until after every non-mutating safety check
                // passes — a strict undo that should refuse must
                // not leave behind directories as a side-effect.
                //
                // Round-7 (Codex F1 P1): before renaming
                // `from_after` back into place, verify it still
                // matches `action.after_hash`. Without this check,
                // a stale or attacker-controlled actions.jsonl can
                // relocate any in-scope file whose original path
                // is currently absent — e.g., move
                // `~/.codex/config` to `~/.codex/oldconfig` or
                // displace operator hook files. Mirrors the
                // WriteFile/DbExec user-edit defense.
                if !action.after_hash.is_empty() {
                    match sha256_stream_no_follow(&from_after) {
                        Ok(cur_hash) => {
                            if cur_hash != action.after_hash {
                                let msg = format!(
                                    "rename source {} no longer matches mutation result (hash {} != recorded after_hash {}); refusing to relocate operator-modified file",
                                    from_after.display(),
                                    cur_hash,
                                    action.after_hash,
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
                                "could not hash rename source {} for after_hash check: {}",
                                from_after.display(),
                                e,
                            ));
                            continue;
                        }
                    }
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
                // Round-9 (Gemini F1 P3): dry-run hits HERE so it
                // mirrors a real run's decision.
                if dry_run {
                    eprintln!(
                        "[dry-run] would rename back: {} -> {}",
                        from_after.display(),
                        restore_to.display()
                    );
                    summary.actions_replayed += 1;
                    continue;
                }
                // Round-8 (Codex F1 P2): parent dir creation
                // moved here — only happens once we've decided
                // the rename will proceed.
                if let Some(parent) = restore_to.parent() {
                    fs::create_dir_all(parent)?;
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
                // Round-7 Gemini F3 (P2): fail-closed on non-ENOENT
                // stat errors. The pre-round-7 derivation of
                // `current_exists` / `current_is_symlink` from a
                // single `Result` silently treated EACCES/ELOOP as
                // "not present as symlink", which downstream
                // branches read as "ok to restore".
                let (current_exists, current_is_symlink) = match fs::symlink_metadata(&target_file)
                {
                    Ok(meta) => (true, meta.file_type().is_symlink()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, false),
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary.failures.push(format!(
                            "could not stat {} for SymlinkAtomic undo: {}",
                            target_file.display(),
                            e,
                        ));
                        continue;
                    }
                };

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
                // Pass-34 wired Op::DbExec at the chokepoint, and the
                // chokepoint took a file-level byte backup of the DB
                // before the exec. The undo path restores that
                // backup — byte-identical main DB file. WAL/SHM
                // siblings are NOT backed up; SQLite is robust to
                // orphan WAL/SHM on open (round-3 review).
                //
                // Round-4 (Codex F2 + Gemini F2): `backup_file.exists()`
                // is the decisive precondition; the target may have
                // been removed by an operator post-fix.
                //
                // Round-5 hardening (Codex F1 / Gemini F3 — symlink
                // defense; Codex F2 / Gemini F5 — atomic restore;
                // Codex F3 / Gemini F4 — streaming hash; Gemini F6 —
                // tight parent-dir mode): the restore now flows
                // through `atomic_restore_db`, which (a) refuses if
                // target is a symlink, (b) streams backup → tempfile
                // while hashing in one pass, (c) verifies the
                // streamed hash equals before_hash BEFORE renaming
                // over the target, and (d) atomically renames the
                // tempfile into place. If the live target still
                // exists and has a recorded after_hash, we also
                // verify the target hasn't been modified by the
                // operator post-fix (matches the WriteFile branch's
                // user-edit defense).
                if !backup_file.exists() {
                    let msg = format!(
                        "{} backup missing for {}; cannot restore",
                        action.op, action.path,
                    );
                    if strict {
                        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
                    }
                    summary.failures.push(msg);
                    continue;
                }
                // Round-6 (Codex F1 P2): fail-closed on metadata
                // errors. The pre-round-6 `.ok()` silently turned
                // EACCES/ELOOP/etc. into "target absent", which
                // bypassed both the symlink defense and the
                // user-edit check below. Now: ENOENT → None,
                // is_symlink → refuse, any other error → bubble up.
                let target_meta = match fs::symlink_metadata(&target_file) {
                    Ok(m) if m.file_type().is_symlink() => {
                        let msg = format!(
                            "target {} is a symlink; refusing to follow (round-5 G2 defense for DB restore)",
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
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary.failures.push(format!(
                            "could not stat {} for symlink/edit check: {}",
                            target_file.display(),
                            e,
                        ));
                        continue;
                    }
                };
                // User-edit defense (Codex F1 / Gemini F3): if the
                // target still exists and we have an after_hash
                // recorded, verify the current target still hashes
                // to that value. If the operator modified the DB
                // after the fix, refuse to clobber their changes.
                if target_meta.is_some() && !action.after_hash.is_empty() {
                    match sha256_stream_no_follow(&target_file) {
                        Ok(cur_hash) => {
                            if cur_hash != action.after_hash {
                                let msg = format!(
                                    "target {} no longer matches mutation result (hash {} != recorded after_hash {}); refusing to clobber operator-modified DB",
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
                                "could not hash {} for after_hash check: {}",
                                target_file.display(),
                                e,
                            ));
                            continue;
                        }
                    }
                }
                if dry_run {
                    eprintln!(
                        "[dry-run] would atomic-restore DB {} from backup {}",
                        target_file.display(),
                        backup_file.display(),
                    );
                    summary.actions_replayed += 1;
                    continue;
                }
                // Recreate parent dir if target's dir was removed.
                // Gemini F6: use 0o700 (sensitive DB storage), not
                // the default 0o755 that DirBuilder applies via
                // create_dir_all.
                if let Some(parent) = target_file.parent()
                    && !parent.exists()
                    && let Err(e) = ensure_parent_dir_strict(parent)
                {
                    if strict {
                        return Err(e);
                    }
                    summary.failures.push(format!(
                        "could not create parent dir for {}: {}",
                        target_file.display(),
                        e,
                    ));
                    continue;
                }
                let restore_mode = action.before_mode.unwrap_or(0o600);
                // Round-6 Gemini F3 (P1): pass after_hash so
                // atomic_restore_db re-checks the live target
                // right before persist, closing the streaming-
                // copy TOCTOU. None when target_meta was None
                // (target was removed; nothing to re-check).
                let expected_after = if target_meta.is_some() && !action.after_hash.is_empty() {
                    Some(action.after_hash.as_str())
                } else {
                    None
                };
                match atomic_restore_db(
                    &backup_file,
                    &target_file,
                    &action.before_hash,
                    expected_after,
                    restore_mode,
                ) {
                    Ok(()) => {
                        summary.actions_replayed += 1;
                    }
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        summary.failures.push(format!(
                            "could not atomic-restore DB {} from backup {}: {}",
                            target_file.display(),
                            backup_file.display(),
                            e,
                        ));
                    }
                }
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

    /// Round-6 Gemini F1 (P0): production `run_undo` enforces
    /// `default_write_scopes()`. Tests use temp dirs that aren't
    /// in the default scope set, so they explicitly grant the
    /// temp root via `run_undo_with_scopes`. Centralizing here
    /// avoids 30+ inline expansions and keeps test intent clean.
    fn test_undo(
        target: &Path,
        run_id: &str,
        dry_run: bool,
        strict: bool,
    ) -> std::io::Result<UndoSummary> {
        run_undo_with_scopes(target, run_id, dry_run, strict, &[target.to_path_buf()])
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
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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
        let _ = test_undo(td.path(), run_id, true, true).unwrap();
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
        let _ = test_undo(td.path(), run_id, false, true).unwrap();
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
        let _ = test_undo(td.path(), run_id, false, true).unwrap();
        assert!(target.exists(), "rename should be reversed");
        assert!(
            !quarantine.exists(),
            "quarantine should be empty after undo"
        );
    }

    #[test]
    fn undo_rename_refuses_when_quarantined_file_was_modified() {
        // Round-7 (Codex F1 P1): if the operator (or an attacker
        // with write access to a quarantined file) changes the
        // bytes at `rename_to` after the doctor moved the
        // original target there, undo must refuse to relocate
        // that modified file back into the operator-visible
        // path. Without this defense, a stale/malicious
        // actions.jsonl entry could relocate any in-scope file
        // whose original path is currently absent.
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        fs::write(&target, b"data\n").unwrap();
        let quarantine = td.path().join("quar").join("victim.txt");
        let run_id = "2026-05-15T01-00-00Z__rename-edit";
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
        // Operator edits the quarantined file's bytes post-fix.
        fs::write(&quarantine, b"operator overwrite\n").unwrap();
        let pre_undo_bytes = fs::read(&quarantine).unwrap();
        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
        );
        // Quarantined file still has the operator's content.
        assert_eq!(fs::read(&quarantine).unwrap(), pre_undo_bytes);
        // Original target stays absent (the rename wasn't reversed).
        assert!(!target.exists());
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
        let err = test_undo(td.path(), "../escape", false, true).unwrap_err();
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

        let err = test_undo(td.path(), run_id, false, true).unwrap_err();
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

        let err = test_undo(td.path(), run_id, false, true).unwrap_err();
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

        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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

        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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

        let result = test_undo(td.path(), run_id, false, true);
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
        let result = test_undo(td.path(), run_id, false, true);
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
        let summary = test_undo(td.path(), run_id, false, false).unwrap();
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
        let result = test_undo(td.path(), run_id, false, true);
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

        let result = test_undo(td.path(), run_id, false, true);
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
        let summary = test_undo(td.path(), run_id, false, false).unwrap();
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
        let result = test_undo(td.path(), run_id, false, true);
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

        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        // Round-6 (Gemini F2 P1): `read_regular_file_no_follow`
        // now flows through `open_regular_file_no_follow` which
        // opens with O_NOFOLLOW, so a symlink at the backup path
        // yields `FilesystemLoop` (ELOOP) rather than the prior
        // `InvalidInput` from the post-open metadata check. Both
        // are correct "refused symlink"; we accept either to keep
        // this test resilient against future helper refactors.
        let err = result.unwrap_err();
        let kind = err.kind();
        #[cfg(unix)]
        let symlink_loop = err.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32);
        #[cfg(not(unix))]
        let symlink_loop = false;
        assert!(
            matches!(kind, std::io::ErrorKind::InvalidInput) || symlink_loop,
            "expected InvalidInput or ELOOP, got {kind:?}",
        );
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

        let result = test_undo(td.path(), run_id, false, true);
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

        let result = test_undo(td.path(), run_id, false, true);
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
            "before_hash": sha256_hex(b"original\n"),
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
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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
            "before_hash": sha256_hex(b"original\n"),
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

        let _ = test_undo(td.path(), run_id, true, true).unwrap();
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
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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

        let summary = test_undo(td.path(), run_id, false, true).unwrap();
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

        // Round-6 (Gemini F1 P0): undo scope must include the
        // outside target. The fix granted `outside.path()` to the
        // chokepoint capabilities; undo must mirror that grant.
        let summary = run_undo_with_scopes(
            repo.path(),
            run_id,
            false,
            true,
            &[repo.path().to_path_buf(), outside.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "outside original\n"
        );
    }

    #[test]
    fn undo_db_exec_restores_when_target_is_missing() {
        // Pass-34 round-4 (Codex F2 + Gemini F2): undo of
        // DbExec/DbMigrate must restore from the backup even
        // when the operator deleted the target between the fix
        // run and the undo. Pre-fix, the branch required both
        // backup_file AND target_file to exist and silently
        // skipped otherwise — which broke the reversibility
        // contract for a plausible recovery scenario.
        use sqlmodel_sqlite::SqliteConnection;
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER);").unwrap();
        drop(conn);
        let pre_bytes = fs::read(&db).unwrap();
        let pre_hash = sha256_hex(&pre_bytes);
        let run_id = "2026-05-14T00-00-00Z__dbexec-missing";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO t VALUES (1);".to_string(),
            },
        )
        .unwrap();
        drop(ctx);
        // Operator deletes the DB between fix and undo.
        fs::remove_file(&db).unwrap();
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(
            summary.actions_replayed, 1,
            "DbExec undo must restore when target is missing (failures: {:?})",
            summary.failures,
        );
        assert!(
            summary.failures.is_empty(),
            "unexpected failures: {:?}",
            summary.failures,
        );
        assert!(db.exists(), "DB must be re-created from backup");
        let restored_bytes = fs::read(&db).unwrap();
        let restored_hash = sha256_hex(&restored_bytes);
        assert_eq!(
            restored_hash, pre_hash,
            "restored DB must hash to the pre-mutation before_hash",
        );
    }

    #[test]
    fn undo_db_exec_detects_hash_mismatch_after_copy() {
        // Pass-34 round-4 (Codex F3): undo of DbExec/DbMigrate
        // must verify the post-restore SHA-256 against
        // action.before_hash. A short copy, partial overwrite,
        // or tampered backup must NOT be recorded as a clean
        // undo.
        use sqlmodel_sqlite::SqliteConnection;
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER);").unwrap();
        drop(conn);
        let run_id = "2026-05-14T00-00-00Z__dbexec-hashmismatch";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO t VALUES (1);".to_string(),
            },
        )
        .unwrap();
        drop(ctx);
        // Tamper with the backup so post-restore hash will not
        // match before_hash. Find the per-run backup of the DB.
        let run_dir = doctor_root(td.path()).join("runs").join(run_id);
        let backups = run_dir.join("backups");
        // Walk the backups dir to find the DB backup. The
        // chokepoint encodes the absolute path with a
        // timestamp-suffix; we just hit the first regular file.
        let mut backup_path = None;
        fn walk(dir: &Path, out: &mut Option<PathBuf>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if out.is_none() {
                    *out = Some(p);
                }
            }
        }
        walk(&backups, &mut backup_path);
        let backup = backup_path.expect("backup must exist");
        // Truncate the backup to corrupt the hash without
        // changing existence.
        fs::write(&backup, b"corrupted backup\n").unwrap();
        // Non-strict mode: failure recorded, summary returned.
        let summary = test_undo(td.path(), run_id, false, false).unwrap();
        assert_eq!(
            summary.actions_replayed, 0,
            "undo must not count tampered-backup restore as success",
        );
        assert!(
            summary.failures.iter().any(|f| f.contains("hash mismatch")),
            "expected hash-mismatch failure, got: {:?}",
            summary.failures,
        );
    }

    #[test]
    fn undo_db_exec_refuses_symlink_at_target() {
        // Round-5 (Codex F1 + Gemini F3): if the target was
        // swapped to a symlink between fix and undo, the restore
        // path must refuse with PermissionDenied — never follow
        // the link and clobber the referent. Mirrors the
        // WriteFile G2 defense.
        use sqlmodel_sqlite::SqliteConnection;
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER);").unwrap();
        drop(conn);
        let run_id = "2026-05-15T00-00-00Z__dbexec-symlink-target";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO t VALUES (1);".to_string(),
            },
        )
        .unwrap();
        drop(ctx);
        // Replace target with symlink to a sensitive file.
        fs::remove_file(&db).unwrap();
        let sensitive = td.path().join("attacker-target.bin");
        fs::write(&sensitive, b"do not overwrite\n").unwrap();
        std::os::unix::fs::symlink(&sensitive, &db).unwrap();
        // Strict mode: must return PermissionDenied.
        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
        );
        // Sensitive file untouched.
        assert_eq!(fs::read(&sensitive).unwrap(), b"do not overwrite\n");
    }

    #[test]
    fn undo_db_exec_refuses_when_target_user_modified() {
        // Round-5 (Codex F1 / Gemini F3 user-edit defense): if the
        // operator modified the DB between fix and undo, the
        // current target's hash no longer matches the recorded
        // after_hash and undo must refuse to clobber their work.
        use sqlmodel_sqlite::SqliteConnection;
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER);").unwrap();
        drop(conn);
        let run_id = "2026-05-15T00-00-00Z__dbexec-user-modified";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO t VALUES (1);".to_string(),
            },
        )
        .unwrap();
        drop(ctx);
        // Operator clobbers the DB with unrelated content
        // post-fix. The replacement's hash ≠ recorded after_hash.
        fs::write(&db, b"operator's new content\n").unwrap();
        let pre_undo_bytes = fs::read(&db).unwrap();
        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
        );
        // Operator's content preserved.
        assert_eq!(fs::read(&db).unwrap(), pre_undo_bytes);
    }

    #[test]
    fn undo_db_exec_leaves_live_target_intact_on_tampered_backup() {
        // Round-5 (Codex F2 / Gemini F5): tampered backup with
        // wrong hash must NOT corrupt the live target. The
        // atomic restore writes to a tempfile, hash-verifies
        // BEFORE renaming, and refuses if mismatched — so the
        // live target is untouched on failure.
        use sqlmodel_sqlite::SqliteConnection;
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER);").unwrap();
        drop(conn);
        let run_id = "2026-05-15T00-00-00Z__dbexec-tampered-backup-atomic";
        let ctx = make_ctx(&td, run_id);
        let _ = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO t VALUES (1);".to_string(),
            },
        )
        .unwrap();
        drop(ctx);
        let pre_undo_bytes = fs::read(&db).unwrap();
        let pre_undo_hash = sha256_hex(&pre_undo_bytes);
        // Find and tamper the backup.
        let run_dir = doctor_root(td.path()).join("runs").join(run_id);
        let backups = run_dir.join("backups");
        fn walk(dir: &Path, out: &mut Option<PathBuf>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if out.is_none() {
                    *out = Some(p);
                }
            }
        }
        let mut backup_path = None;
        walk(&backups, &mut backup_path);
        let backup = backup_path.expect("backup must exist");
        fs::write(&backup, b"tampered backup that wont match before_hash\n").unwrap();
        // Non-strict: failure recorded, live target untouched.
        let summary = test_undo(td.path(), run_id, false, false).unwrap();
        assert_eq!(summary.actions_replayed, 0);
        assert!(
            summary.failures.iter().any(|f| f.contains("hash mismatch")),
            "expected hash-mismatch failure: {:?}",
            summary.failures,
        );
        // Critical: live target byte-identical to pre-undo state.
        let post_undo_bytes = fs::read(&db).unwrap();
        let post_undo_hash = sha256_hex(&post_undo_bytes);
        assert_eq!(
            post_undo_hash, pre_undo_hash,
            "atomic restore must not corrupt the live target on backup-tamper failure",
        );
    }

    #[test]
    fn undo_refuses_when_concurrent_fix_holds_target_lock() {
        // Round-10 (round-7 Codex F2 / Gemini F6): undo must
        // acquire the per-path `.<basename>.doctor-lock` so a
        // concurrent `am doctor --fix` can't interleave a
        // hash/write step against the same target. Simulated by
        // manually grabbing the lock before invoking undo;
        // strict mode must surface WouldBlock.
        use fs2::FileExt;
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-15T02-00-00Z__concurrent-fix";
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
        // Concurrent fix holds the per-path lock.
        let lock_path = td.path().join(".victim.txt.doctor-lock");
        let competitor = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        competitor.try_lock_exclusive().unwrap();
        // Strict undo must surface WouldBlock.
        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        // Target unchanged (still in post-fix state).
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        // Release the competitor lock; undo succeeds on retry.
        FileExt::unlock(&competitor).unwrap();
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    }

    #[test]
    fn undo_dry_run_skips_target_lock_acquisition() {
        // Round-10: dry-run undo doesn't mutate anything, so
        // there's no need to take the per-path lock. A concurrent
        // fix holding the lock must NOT block dry-run preview.
        use fs2::FileExt;
        let td = TempDir::new().unwrap();
        let target = td.path().join("preview.txt");
        fs::write(&target, b"original\n").unwrap();
        let run_id = "2026-05-15T02-00-01Z__dryrun-lock-skip";
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
        let lock_path = td.path().join(".preview.txt.doctor-lock");
        let competitor = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        competitor.try_lock_exclusive().unwrap();
        let summary = test_undo(td.path(), run_id, true, true).unwrap();
        // Dry-run reports 1 replayable action without mutating.
        assert_eq!(summary.actions_replayed, 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        FileExt::unlock(&competitor).unwrap();
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
        let result = test_undo(td.path(), run_id, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        // Release the competitor's lock.
        FileExt::unlock(&competitor).unwrap();
        // Now run_undo should succeed.
        let summary = test_undo(td.path(), run_id, false, true).unwrap();
        assert_eq!(summary.actions_replayed, 1);
    }
}
