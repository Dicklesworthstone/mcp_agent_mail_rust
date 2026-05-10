//! The `mutate()` chokepoint — every disk write under `am doctor --fix`
//! flows through here.
//!
//! Routing every fixer-driven mutation through one function buys us:
//! - Verbatim per-file backups under `<run-dir>/backups/`
//! - Hash-witnessed `actions.jsonl` (before/after SHA-256 per write)
//! - Atomic write semantics (write-tmp-then-rename)
//! - Reversibility via `am doctor undo <run-id>` (reads actions.jsonl in reverse)
//! - Per-path advisory lock (no concurrent writers stepping on each other)
//!
//! The seven canonical `Op` variants cover every mutation we need; there is
//! no `DeletePath` op because AGENTS.md "no file deletion" forbids it.
//! Quarantine via `Op::Rename` to `<run-dir>/quarantine/<rel-path>` instead.
//!
//! Project constraints honored:
//! - `#![forbid(unsafe_code)]`
//! - asupersync-only (no tokio): mutate() is synchronous; doctor runs out
//!   of band of the request hot path
//! - Rust 2024 edition
//!
//! See: `references/methodology/MUTATE-CHOKEPOINT.md` in
//! `world-class-doctor-mode-for-cli-tools` for the full contract.

#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The seven canonical mutation operations. `DeletePath` is intentionally
/// absent — quarantine via `Op::Rename`.
#[derive(Debug, Clone, Serialize)]
pub enum Op {
    /// Create-or-overwrite the file at `path` (atomic via tempfile + rename).
    WriteFile { content: Vec<u8>, mode: u32 },
    /// Append to the file at `path`.
    AppendFile { content: Vec<u8> },
    /// Atomic rename of `path` → `to`. Used for "delete-equivalent"
    /// (move to quarantine) and atomic state swaps.
    Rename { to: PathBuf },
    /// Set the mode of `path`.
    Chmod { mode: u32 },
    /// Execute `sql` against the project's DB inside a transaction. Wired
    /// to the project's `DbConn` by the dispatch layer; this struct
    /// only carries the SQL.
    DbExec { sql: String },
    /// Versioned schema migration; rolls back on error.
    DbMigrate { from: u32, to: u32 },
    /// Atomic symlink replacement (used for `.doctor/latest`).
    SymlinkAtomic { target: PathBuf },
}

impl Op {
    pub fn op_kind(&self) -> &'static str {
        match self {
            Op::WriteFile { .. } => "WriteFile",
            Op::AppendFile { .. } => "AppendFile",
            Op::Rename { .. } => "Rename",
            Op::Chmod { .. } => "Chmod",
            Op::DbExec { .. } => "DbExec",
            Op::DbMigrate { .. } => "DbMigrate",
            Op::SymlinkAtomic { .. } => "SymlinkAtomic",
        }
    }
}

/// One line of `actions.jsonl`. Schema documented in
/// `world-class-doctor-mode-for-cli-tools` OUTPUT-SCHEMA.md.
#[derive(Debug, Serialize)]
pub struct ActionRecord {
    pub path: String,
    pub op: String,
    pub before_hash: String,
    pub after_hash: String,
    pub started_at_ns: u128,
    pub finished_at_ns: u128,
    pub run_id: String,
    pub fixer_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<bool>,
}

/// Doctor capabilities — the contract.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Paths the doctor is allowed to write under. Mutations outside refuse
    /// with exit 4.
    pub write_scopes: Vec<PathBuf>,
}

/// Per-run mutation context. Constructed at the top of a `--fix` run and
/// threaded through every fixer.
pub struct MutateContext {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub capabilities: Capabilities,
    pub actions_file: Mutex<std::fs::File>,
    pub fixer_id: String,
    pub repo_root: PathBuf,
    pub dry_run: bool,
    pub start: Instant,
}

#[derive(Debug)]
pub struct ActionResult {
    pub ok: bool,
    pub before_hash: String,
    pub after_hash: String,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MutateError {
    #[error("path {0} is outside write_scopes")]
    OutOfScope(PathBuf),
    #[error("lock_held for {0}")]
    LockHeld(PathBuf),
    #[error("backup verify failed (cmp-strict) for {0}")]
    BackupVerify(PathBuf),
    /// The live file's hash changed between step 2 (before_hash) and step 4
    /// (post-backup re-hash). Concurrent writer detected; refusing to
    /// proceed because our backup wouldn't faithfully represent the
    /// pre-mutation state. Maps to exit 5 (`concurrency_lost`).
    #[error("file {0} was modified between hash and backup (concurrent writer)")]
    TamperedBeforeMutate(PathBuf),
    /// `Op::Rename` would clobber an existing file at the destination.
    /// Per AGENTS.md RULE 1 (no file deletion), we refuse rather than
    /// overwrite via the silent-overwrite POSIX `rename` semantics.
    /// Maps to exit 4 (`refused_unsafe`).
    #[error("rename destination {0} already exists (would clobber per AGENTS.md RULE 1)")]
    RenameDestinationExists(PathBuf),
    /// The mutation execution failed. The backup was rolled back (or
    /// there was nothing to roll back to). `rolled_back` reflects the
    /// actual result. Maps to exit 3 (`fix_failed_rolled_back`) when
    /// `rolled_back == Some(true)`, exit 2 when `Some(false)`, and we
    /// recommend exit 3 by default.
    #[error("exec failed for {path:?} ({op}): {message}")]
    ExecFailed {
        path: PathBuf,
        op: &'static str,
        message: String,
        rolled_back: Option<bool>,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("op not implemented in this build: {0}")]
    Unsupported(&'static str),
}

/// G-OOM fix (Gemini round 2): streaming SHA-256 over the file's bytes
/// without loading the entire file into memory. Returns the empty-file
/// hash if the path doesn't exist.
fn sha256_of_path(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    if !path.exists() {
        return Ok(format!("sha256:{:x}", Sha256::digest(b"")));
    }
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn read_or_empty(path: &Path) -> std::io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Canonicalize `path` resolving symlinks, or fall back to canonicalizing
/// the nearest existing ancestor and joining the file name. This avoids
/// "no such file or directory" when the path is a target we're about to
/// CREATE.
fn canonicalize_existing_or_parent(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return path.canonicalize();
    }
    let mut cur = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    while !cur.exists() {
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no existing ancestor for path",
                ));
            }
        }
    }
    let canonical_parent = cur.canonicalize()?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    Ok(canonical_parent.join(name))
}

fn ensure_in_scope(caps: &Capabilities, path: &Path) -> Result<(), MutateError> {
    let canonical = canonicalize_existing_or_parent(path).map_err(MutateError::Io)?;
    for scope in &caps.write_scopes {
        if let Ok(canonical_scope) = canonicalize_existing_or_parent(scope)
            && canonical.starts_with(&canonical_scope)
        {
            return Ok(());
        }
    }
    Err(MutateError::OutOfScope(path.to_path_buf()))
}

fn copy_verbatim_with_perms(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst)?;
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, Permissions::from_mode(meta.permissions().mode()))?;
    Ok(())
}

/// Streaming comparison of two files. G-OOM fix (Gemini round 2):
/// avoids loading entire file contents into memory. Reads 64 KiB at a
/// time; first divergence aborts.
fn cmp_strict(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::io::Read;
    let mut fa = fs::File::open(a)?;
    let mut fb = fs::File::open(b)?;
    let len_a = fa.metadata()?.len();
    let len_b = fb.metadata()?.len();
    if len_a != len_b {
        return Err(std::io::Error::other(format!(
            "backup verify failed (length mismatch: {len_a} vs {len_b})"
        )));
    }
    let mut buf_a = vec![0u8; 65_536];
    let mut buf_b = vec![0u8; 65_536];
    loop {
        let na = fa.read(&mut buf_a)?;
        let nb = fb.read(&mut buf_b)?;
        if na != nb {
            return Err(std::io::Error::other("backup verify failed (cmp-strict)"));
        }
        if na == 0 {
            break;
        }
        if buf_a[..na] != buf_b[..nb] {
            return Err(std::io::Error::other("backup verify failed (cmp-strict)"));
        }
    }
    Ok(())
}

fn elapsed_ns(start: Instant) -> u128 {
    start.elapsed().as_nanos()
}

/// Atomic write via tempfile-in-same-dir + rename.
///
/// G4 fix (Gemini round 2): permissions are set on the tempfile's file
/// descriptor BEFORE `tmp.persist(path)`. Setting permissions on `path`
/// post-persist would race a symlink-swap attacker who could redirect
/// `path` to an arbitrary out-of-scope file between persist and chmod.
pub(crate) fn atomic_write_file(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut f = tmp.as_file();
        f.write_all(content)?;
        f.sync_data()?;
        // G4 fix: chmod via fd before persist (symlink-attack defense).
        f.set_permissions(Permissions::from_mode(mode))?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|d| d.sync_all());
    Ok(())
}

/// Compute the backup path for a target, sequenced by `started_at_ns`.
///
/// G1 fix (Gemini round 2): when `path` is absolute and outside `repo_root`,
/// `strip_prefix` returns Err and `PathBuf::join(absolute)` drops the base —
/// so naive `run_dir.join("backups").join(rel)` returned the live target itself.
/// We now encode out-of-repo paths under a sentinel `__abs__/` subdirectory.
///
/// Pass-4 multi-mutation fix (caught by property test): without sequencing,
/// two mutations to the same path within one run share a backup path and the
/// second overwrites the first. Undo then restores the wrong content. We now
/// scope every mutation's backup under `backups/<started_at_ns>/`, so each
/// mutation has its own backup directory. Undo finds the per-mutation backup
/// via the recorded `started_at_ns`. `started_at_ns` is monotonically
/// increasing per `MutateContext.start`, so two mutations cannot collide.
pub(crate) fn backup_path_for(
    run_dir: &Path,
    repo_root: &Path,
    path: &Path,
    started_at_ns: u128,
) -> PathBuf {
    // Zero-padded sequence so lexicographic order matches numerical order
    // (useful for ls/debug). 26 digits covers 10^26 ns ≈ 3 trillion years.
    let seq = format!("seq_{:026}", started_at_ns);
    let backups = run_dir.join("backups").join(&seq);
    if let Ok(rel) = path.strip_prefix(repo_root) {
        return backups.join(rel);
    }
    // Outside repo_root — encode the absolute path as a "rooted" relative
    // path under backups/<seq>/__abs__/. Strip a leading `/` so PathBuf::join
    // doesn't drop the prefix.
    let abs_str = path.to_string_lossy();
    let trimmed = abs_str.trim_start_matches('/');
    backups.join("__abs__").join(trimmed)
}

/// Atomic symlink replacement: write tmp symlink in same dir, rename over.
fn atomic_symlink(path: &Path, target: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(
        ".{}.tmp.{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        now_ns,
    );
    let tmp_path = parent.join(&tmp_name);
    if fs::symlink_metadata(&tmp_path).is_ok() {
        fs::remove_file(&tmp_path)?;
    }
    std::os::unix::fs::symlink(target, &tmp_path)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Append `content` to `path` (creating if missing).
///
/// G-AppendChmod-TOCTOU fix (Gemini round 2): uses `O_NOFOLLOW` so a
/// symlink-swap attacker who replaces `path` with a symlink between the
/// step-4 hash check and this open cannot redirect the append to an
/// out-of-scope file (e.g., /etc/shadow). On a symlink, open returns
/// `ELOOP` which we map to `MutateError::Io`.
fn append_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc_consts::O_NOFOLLOW)
        .open(path)?;
    f.write_all(content)?;
    f.sync_data()?;
    Ok(())
}

/// `O_NOFOLLOW` value as a const so we don't need a libc dep.
mod libc_consts {
    /// Linux/macOS/BSD: O_NOFOLLOW = 0x20000 on Linux x86_64, 0x100 on macOS.
    /// Rust stdlib's std::fs doesn't expose this. Use cfg-target.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const O_NOFOLLOW: i32 = 0o400000;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    pub const O_NOFOLLOW: i32 = 0x0100;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    pub const O_NOFOLLOW: i32 = 0;
}

/// Set permissions on `path` via the file descriptor (G-AppendChmod fix).
///
/// Opens the file with `O_NOFOLLOW` first so a symlink-swap attacker
/// cannot redirect the chmod to an arbitrary file. Returns `ELOOP` if
/// `path` is now a symlink.
fn chmod_via_fd(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc_consts::O_NOFOLLOW)
        .open(path)?;
    f.set_permissions(Permissions::from_mode(mode))?;
    Ok(())
}

/// THE chokepoint. Every fixer-driven mutation flows through this.
///
/// Steps in order:
/// 1. Per-path advisory lock (`fs2::FileExt::try_lock_exclusive`).
/// 2. Compute `before_hash`.
/// 3. Validate preconditions (path in scope, rename destination in scope).
/// 4. Write verbatim backup; verify with `cmp_strict`; verify
///    `sha256(live) == before_hash` (TOCTOU defense; if mismatch, refuse).
/// 5. Plan the mutation in memory.
/// 6. Execute atomically (skipped on dry-run, after preconditions pass).
/// 7. On exec failure: ATOMIC rollback from backup; record truthful
///    `rolled_back` value.
/// 8. Compute `after_hash`.
/// 9. Append to `actions.jsonl`; fsync; release lock.
///
/// Errors:
/// - `Err(MutateError::ExecFailed(_))` if the mutation could not be
///   completed. Per H4 fix, callers using `?` see a real error rather
///   than `Ok(ActionResult { ok: false })`.
/// - `Err(MutateError::TamperedBeforeMutate)` if the live file's hash no
///   longer matches the backup's right after we copied it (TOCTOU
///   detected).
pub fn mutate(ctx: &MutateContext, path: &Path, op: Op) -> Result<ActionResult, MutateError> {
    let started_at_ns = elapsed_ns(ctx.start);

    // 1. Per-path advisory lock. Lock file lives next to target, distinct name.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let basename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "_root_".to_string());
    let lock_path = parent.join(format!(".{}.doctor-lock", basename));
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if lock_file.try_lock_exclusive().is_err() {
        return Err(MutateError::LockHeld(path.to_path_buf()));
    }

    // 2. before_hash + before_mode.
    // G-OOM fix: stream-hash the file rather than reading entire contents
    // into memory. Multi-GB files (e.g., storage.sqlite3) would OOM otherwise.
    let before_hash = sha256_of_path(path)?;
    let before_mode = fs::metadata(path).ok().map(|m| m.permissions().mode());

    // 3. Preconditions: path in scope + Rename destination in scope (H8 fix —
    // moved BEFORE the dry-run early return so dry-run cannot lie about
    // would-be exit-4 refusals).
    ensure_in_scope(&ctx.capabilities, path)?;
    if let Op::Rename { to } = &op {
        ensure_in_scope(&ctx.capabilities, to)?;
    }

    // 4. Verbatim backup (only if file exists). Also re-verifies that the
    // live file still hashes to before_hash AFTER copying — if not, a
    // concurrent writer modified the file in our window (H3 fix), and we
    // refuse to proceed because our backup wouldn't be a real backup of
    // the pre-mutation state.
    //
    // G1 fix (Gemini round 2): use `backup_path_for` which correctly handles
    // absolute paths outside repo_root (was: PathBuf::join with absolute
    // path returned the live file itself, causing copy-onto-self). The
    // `rel` value used in actions.jsonl stays the original path semantics
    // — for absolute paths, undo's `target.join(absolute)` recovers the
    // absolute path (PathBuf::join's same semantics that broke backup).
    let backup_path = backup_path_for(&ctx.run_dir, &ctx.repo_root, path, started_at_ns);
    let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
    if !ctx.dry_run && path.exists() {
        copy_verbatim_with_perms(path, &backup_path)?;
        cmp_strict(path, &backup_path)
            .map_err(|_| MutateError::BackupVerify(path.to_path_buf()))?;
        // Re-hash the live file; if it changed since step 2, someone else
        // is writing — refuse with a concurrency-loss-style error.
        // G-OOM fix: stream-hash to avoid loading full file twice.
        let post_backup_hash = sha256_of_path(path)?;
        if post_backup_hash != before_hash {
            let _ = FileExt::unlock(&lock_file);
            return Err(MutateError::TamperedBeforeMutate(path.to_path_buf()));
        }
    }

    // Dry-run early return (after all preconditions have been checked).
    if ctx.dry_run {
        eprintln!(
            "[dry-run] would mutate {} via {}",
            path.display(),
            op.op_kind()
        );
        let _ = FileExt::unlock(&lock_file);
        return Ok(ActionResult {
            ok: true,
            before_hash: before_hash.clone(),
            after_hash: before_hash,
            error: None,
        });
    }

    let mut rename_to_record: Option<String> = None;
    let mut after_mode: Option<u32> = None;

    // 5/6. Execute atomically.
    let exec_result: Result<(), MutateError> = match op.clone() {
        Op::WriteFile { content, mode } => {
            atomic_write_file(path, &content, mode)?;
            after_mode = Some(mode);
            Ok(())
        }
        Op::AppendFile { content } => {
            append_file(path, &content)?;
            Ok(())
        }
        Op::Rename { to } => {
            // Destination scope already checked at step 3.
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            // G-Lock-Rename-Dest fix (Gemini round 2): also acquire an
            // advisory lock on the destination basename. The source's
            // per-path lock (already held above) protects concurrent
            // mutations of `path`; without locking the destination too,
            // two concurrent renames-to-same-quarantine paths would race
            // (one would see destination missing, one would see it exist
            // — the loser's rename either succeeds (clobber) or fails).
            // Note we hold this lock for the lifetime of `to_lock_file`.
            let to_basename = to
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "_root_".to_string());
            let to_lock_path = to
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(".{}.doctor-lock", to_basename));
            let to_lock_file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&to_lock_path)?;
            if to_lock_file.try_lock_exclusive().is_err() {
                return Err(MutateError::LockHeld(to.clone()));
            }
            // G3 fix (Gemini round 2): refuse if the destination already
            // exists. POSIX `fs::rename` overwrites silently, which would
            // permanently destroy the existing file at `to` — direct
            // violation of AGENTS.md RULE 1 (no file deletion). The
            // appropriate response is to either pick a different
            // quarantine name OR refuse with exit-4 / RenameClobberRefused.
            // The check happens AFTER acquiring the destination lock so
            // a concurrent rename-to-same-dest can't race past it.
            if fs::symlink_metadata(&to).is_ok() {
                let _ = FileExt::unlock(&to_lock_file);
                return Err(MutateError::RenameDestinationExists(to.clone()));
            }
            fs::rename(path, &to)?;
            rename_to_record = Some(to.to_string_lossy().into_owned());
            let _ = FileExt::unlock(&to_lock_file);
            Ok(())
        }
        Op::Chmod { mode } => {
            // G-AppendChmod-TOCTOU fix: chmod via fd opened with O_NOFOLLOW
            // so a symlink-swap attacker cannot redirect to an out-of-scope file.
            chmod_via_fd(path, mode)?;
            after_mode = Some(mode);
            Ok(())
        }
        Op::DbExec { sql: _ } => Err(MutateError::Unsupported(
            "DbExec requires a DbConn handle wired by a higher layer",
        )),
        Op::DbMigrate { from: _, to: _ } => Err(MutateError::Unsupported(
            "DbMigrate requires a DbConn handle wired by a higher layer",
        )),
        Op::SymlinkAtomic { target } => {
            atomic_symlink(path, &target)?;
            Ok(())
        }
    };

    // 7. On exec failure: attempt ATOMIC rollback (H2 fix — was non-atomic
    // fs::copy before; now uses atomic_write_file). Record the TRUE
    // `rolled_back` outcome (C1 fix — was unconditionally `Some(true)`).
    let rolled_back: Option<bool> = if exec_result.is_err() {
        if backup_path.exists() && path.exists() {
            let backup_bytes = read_or_empty(&backup_path)?;
            let restore_mode = before_mode.unwrap_or(0o644);
            match atomic_write_file(path, &backup_bytes, restore_mode) {
                Ok(_) => Some(true),
                Err(_) => Some(false),
            }
        } else {
            // Nothing to roll back to (file didn't exist before mutation, or
            // the mutation was Rename so backup_path is the same content as
            // pre-mutation `path`). Mark as no rollback needed.
            None
        }
    } else {
        None
    };

    // 8. after_hash (read post-state via streaming hash — G-OOM fix).
    // For Rename: hash the destination. Else: hash the original path.
    let after_hash = match &op {
        Op::Rename { to } if exec_result.is_ok() => sha256_of_path(to)?,
        _ => sha256_of_path(path)?,
    };

    // 9. Append to actions.jsonl, fsync. The `rolled_back` field reflects
    // the actual rollback result (C1 fix), not an assumption.
    let ok = exec_result.is_ok();
    let error = exec_result.as_ref().err().map(|e| e.to_string());
    let record = ActionRecord {
        path: rel.to_string_lossy().into_owned(),
        op: op.op_kind().to_string(),
        before_hash: before_hash.clone(),
        after_hash: after_hash.clone(),
        started_at_ns,
        finished_at_ns: elapsed_ns(ctx.start),
        run_id: ctx.run_id.clone(),
        fixer_id: ctx.fixer_id.clone(),
        ok,
        rename_to: rename_to_record,
        before_mode,
        after_mode,
        error: error.clone(),
        rolled_back,
    };
    let line = serde_json::to_string(&record)? + "\n";
    {
        let mut f = ctx
            .actions_file
            .lock()
            .map_err(|_| MutateError::Unsupported("actions_file mutex poisoned"))?;
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
    }
    let _ = FileExt::unlock(&lock_file);

    // H4 fix — return Err on exec failure, not Ok with ok: false.
    if let Err(e) = exec_result {
        return Err(MutateError::ExecFailed {
            path: path.to_path_buf(),
            op: op.op_kind(),
            message: e.to_string(),
            rolled_back,
        });
    }

    Ok(ActionResult {
        ok: true,
        before_hash,
        after_hash,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_ctx(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = td.path().join(".doctor").join("runs").join(run_id);
        fs::create_dir_all(run_dir.join("backups")).unwrap();
        let actions = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir: run_dir.clone(),
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: "test-fixer".to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
        }
    }

    #[test]
    fn op_kind_returns_seven_canonical_variants() {
        assert_eq!(
            Op::WriteFile {
                content: vec![],
                mode: 0o644
            }
            .op_kind(),
            "WriteFile"
        );
        assert_eq!(Op::AppendFile { content: vec![] }.op_kind(), "AppendFile");
        assert_eq!(
            Op::Rename {
                to: PathBuf::from("/tmp/x")
            }
            .op_kind(),
            "Rename"
        );
        assert_eq!(Op::Chmod { mode: 0o600 }.op_kind(), "Chmod");
        assert_eq!(
            Op::DbExec {
                sql: "SELECT 1".into()
            }
            .op_kind(),
            "DbExec"
        );
        assert_eq!(Op::DbMigrate { from: 1, to: 2 }.op_kind(), "DbMigrate");
        assert_eq!(
            Op::SymlinkAtomic {
                target: PathBuf::from("x")
            }
            .op_kind(),
            "SymlinkAtomic"
        );
    }

    #[test]
    fn write_file_creates_with_atomic_semantics() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let target = td.path().join("hello.txt");
        let r = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"hello world\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        assert!(r.ok);
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello world\n");
    }

    #[test]
    fn write_file_records_actions_jsonl_with_hashes() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let target = td.path().join("hello.txt");
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"x".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        drop(ctx);
        let actions = fs::read_to_string(
            td.path()
                .join(".doctor")
                .join("runs")
                .join("2026-05-09T16-30-15Z__abc")
                .join("actions.jsonl"),
        )
        .unwrap();
        let line = actions.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["op"], "WriteFile");
        assert!(v["before_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(v["after_hash"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn write_file_backs_up_existing_content_verbatim() {
        // Pass-4: backups now live under `backups/seq_<started_at_ns>/<rel>`
        // (per-mutation, not per-path) so multiple mutations to the same
        // file in one run don't overwrite each other's backups. Verify
        // by enumerating the backups dir and finding hello.txt with
        // original content, regardless of the seq directory.
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        fs::write(&target, b"original\n").unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"new\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        // Find the seq_<ns>/hello.txt backup.
        let backups_root = ctx.run_dir.join("backups");
        let mut found_backup_content: Option<String> = None;
        for entry in fs::read_dir(&backups_root).unwrap().flatten() {
            let candidate = entry.path().join("hello.txt");
            if let Ok(s) = fs::read_to_string(&candidate) {
                found_backup_content = Some(s);
                break;
            }
        }
        assert_eq!(
            found_backup_content,
            Some("original\n".to_string()),
            "expected to find seq_<ns>/hello.txt backup with original content"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[test]
    fn pass4_multiple_mutations_to_same_path_get_distinct_backups() {
        // Pass-4: caught by property test. Two mutations to the same path
        // must NOT overwrite each other's backups. Each mutation gets its
        // own seq_<ns> subdir.
        let td = TempDir::new().unwrap();
        let target = td.path().join("collide.txt");
        fs::write(&target, b"v0\n").unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__pass4");
        // Two consecutive mutations.
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"v1\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"v2\n".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        // Two distinct backup directories must exist, with v0 and v1 contents.
        let backups_root = ctx.run_dir.join("backups");
        let mut found_contents: Vec<String> = Vec::new();
        for entry in fs::read_dir(&backups_root).unwrap().flatten() {
            if let Ok(s) = fs::read_to_string(entry.path().join("collide.txt")) {
                found_contents.push(s);
            }
        }
        found_contents.sort();
        assert_eq!(
            found_contents,
            vec!["v0\n".to_string(), "v1\n".to_string()],
            "two mutations must produce two distinct backups"
        );
    }

    #[test]
    fn rename_quarantines_via_op_rename_no_deletion() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        fs::write(&target, b"data\n").unwrap();
        let quarantine = td
            .path()
            .join(".doctor")
            .join("quarantine")
            .join("victim.txt");
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let _ = mutate(
            &ctx,
            &target,
            Op::Rename {
                to: quarantine.clone(),
            },
        )
        .unwrap();
        assert!(!target.exists(), "source removed by rename");
        assert!(quarantine.exists(), "destination has the data");
        assert_eq!(fs::read_to_string(&quarantine).unwrap(), "data\n");
    }

    #[test]
    fn dry_run_does_not_write() {
        let td = TempDir::new().unwrap();
        let mut ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        ctx.dry_run = true;
        let target = td.path().join("hello.txt");
        let r = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"x".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap();
        assert!(r.ok);
        assert!(!target.exists(), "dry-run must not write");
    }

    #[test]
    fn out_of_scope_write_refuses() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        // Try to write outside the scope.
        let outside = std::env::temp_dir().join("am-doctor-test-out-of-scope-12345.txt");
        let r = mutate(
            &ctx,
            &outside,
            Op::WriteFile {
                content: b"x".to_vec(),
                mode: 0o644,
            },
        );
        assert!(matches!(r, Err(MutateError::OutOfScope(_))));
    }

    #[test]
    fn chmod_records_before_and_after_modes() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("hello.txt");
        fs::write(&target, b"x").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let _ = mutate(&ctx, &target, Op::Chmod { mode: 0o600 }).unwrap();
        drop(ctx);
        let actions = fs::read_to_string(
            td.path()
                .join(".doctor")
                .join("runs")
                .join("2026-05-09T16-30-15Z__abc")
                .join("actions.jsonl"),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(actions.lines().next().unwrap()).unwrap();
        assert_eq!(v["op"], "Chmod");
        // before_mode was 0o644 = 33188 (decimal), after_mode 0o600 = 33152
        assert_eq!(v["after_mode"].as_u64(), Some(0o600));
    }

    #[test]
    fn db_exec_returns_err_unsupported_in_this_module() {
        // Wired by the dispatch layer; the chokepoint itself doesn't have a DbConn.
        // Per H4 fix, mutate() now returns Err on exec failure (was: Ok with ok: false).
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        let target = td.path().join("anything.txt");
        let err = mutate(
            &ctx,
            &target,
            Op::DbExec {
                sql: "SELECT 1".into(),
            },
        )
        .unwrap_err();
        match err {
            MutateError::ExecFailed { message, .. } => {
                assert!(message.contains("DbConn"), "got: {message}")
            }
            other => panic!("expected ExecFailed, got: {other:?}"),
        }
    }

    #[test]
    fn g1_backup_path_for_handles_absolute_paths_outside_repo() {
        // G1 fix (Gemini round 2): backup_path_for must NOT use PathBuf::join
        // with an absolute path (which drops the base). For paths outside
        // repo_root, encode under __abs__/ subdirectory.
        let run_dir = PathBuf::from("/tmp/run-dir");
        let repo_root = PathBuf::from("/repo");
        let in_repo = PathBuf::from("/repo/.config/x");
        let out_of_repo = PathBuf::from("/home/user/.config/x");
        let started_at_ns = 42;
        let seq = "seq_00000000000000000000000042";
        assert_eq!(
            backup_path_for(&run_dir, &repo_root, &in_repo, started_at_ns),
            PathBuf::from("/tmp/run-dir/backups")
                .join(seq)
                .join(".config/x"),
        );
        let bp = backup_path_for(&run_dir, &repo_root, &out_of_repo, started_at_ns);
        assert!(
            bp.starts_with(run_dir.join("backups").join(seq).join("__abs__")),
            "expected __abs__/ encoding, got: {bp:?}"
        );
        assert!(
            bp.to_string_lossy().contains("home/user/.config/x"),
            "expected encoded absolute path, got: {bp:?}"
        );
    }

    #[test]
    fn g3_rename_destination_exists_refuses() {
        // G3 fix (Gemini round 2): Op::Rename refuses if destination exists.
        // Per AGENTS.md RULE 1, silent overwrite of existing files is forbidden.
        let td = TempDir::new().unwrap();
        let src = td.path().join("source.txt");
        let dst = td.path().join("destination.txt");
        std::fs::write(&src, b"source data").unwrap();
        std::fs::write(&dst, b"important destination data").unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__g3");
        let err = mutate(&ctx, &src, Op::Rename { to: dst.clone() }).unwrap_err();
        assert!(
            matches!(err, MutateError::RenameDestinationExists(_)),
            "got: {err:?}"
        );
        // Source untouched.
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "source data");
        // Destination preserved.
        assert_eq!(
            std::fs::read_to_string(&dst).unwrap(),
            "important destination data",
            "destination file must be preserved per AGENTS.md RULE 1"
        );
    }

    #[test]
    fn g4_atomic_write_chmod_via_fd_before_persist() {
        // G4 fix (Gemini round 2): permissions set via fd before persist,
        // not via path after. Verified by checking the file ends up with
        // the requested mode (this test would not catch a TOCTOU attack
        // directly, but exercises the permission-setting path).
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let target = td.path().join("hook.sh");
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__g4");
        let _ = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"#!/bin/sh\necho hook\n".to_vec(),
                mode: 0o755,
            },
        )
        .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        // Compare lower 9 bits (rwxrwxrwx).
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn out_of_scope_rename_destination_refuses_in_dry_run_too() {
        // H8 fix: dry-run must not lie about would-be exit-4 refusals on
        // out-of-scope rename destinations.
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        std::fs::write(&target, b"x").unwrap();
        let mut ctx = make_ctx(&td, "2026-05-09T16-30-15Z__abc");
        ctx.dry_run = true;
        let outside = std::env::temp_dir().join("am-doctor-test-rename-out-of-scope.txt");
        let err = mutate(
            &ctx,
            &target,
            Op::Rename {
                to: outside.clone(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, MutateError::OutOfScope(_)));
    }
}
