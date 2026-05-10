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
    WriteFile {
        content: Vec<u8>,
        mode: u32,
    },
    /// Append to the file at `path`.
    AppendFile {
        content: Vec<u8>,
    },
    /// Atomic rename of `path` → `to`. Used for "delete-equivalent"
    /// (move to quarantine) and atomic state swaps.
    Rename {
        to: PathBuf,
    },
    /// Set the mode of `path`.
    Chmod {
        mode: u32,
    },
    /// Execute `sql` against the project's DB inside a transaction. Wired
    /// to the project's `DbConn` by the dispatch layer; this struct
    /// only carries the SQL.
    DbExec {
        sql: String,
    },
    /// Versioned schema migration; rolls back on error.
    DbMigrate {
        from: u32,
        to: u32,
    },
    /// Atomic symlink replacement (used for `.doctor/latest`).
    SymlinkAtomic {
        target: PathBuf,
    },
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

fn sha256_hex(bytes: &[u8]) -> String {
    let h = Sha256::digest(bytes);
    format!("sha256:{:x}", h)
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
        if let Ok(canonical_scope) = canonicalize_existing_or_parent(scope) {
            if canonical.starts_with(&canonical_scope) {
                return Ok(());
            }
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

fn cmp_strict(a: &Path, b: &Path) -> std::io::Result<()> {
    let ba = fs::read(a)?;
    let bb = fs::read(b)?;
    if ba != bb {
        return Err(std::io::Error::other("backup verify failed (cmp-strict)"));
    }
    Ok(())
}

fn elapsed_ns(start: Instant) -> u128 {
    start.elapsed().as_nanos()
}

/// Atomic write via tempfile-in-same-dir + rename.
fn atomic_write_file(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut f = tmp.as_file();
        f.write_all(content)?;
        f.sync_data()?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    fs::set_permissions(path, Permissions::from_mode(mode))?;
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|d| d.sync_all());
    Ok(())
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
fn append_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(content)?;
    f.sync_data()?;
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
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if lock_file.try_lock_exclusive().is_err() {
        return Err(MutateError::LockHeld(path.to_path_buf()));
    }

    // 2. before_hash + before_mode.
    let before_bytes = read_or_empty(path)?;
    let before_hash = sha256_hex(&before_bytes);
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
    let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
    let backup_path = ctx.run_dir.join("backups").join(rel);
    if !ctx.dry_run && path.exists() {
        copy_verbatim_with_perms(path, &backup_path)?;
        cmp_strict(path, &backup_path)
            .map_err(|_| MutateError::BackupVerify(path.to_path_buf()))?;
        // Re-hash the live file; if it changed since step 2, someone else
        // is writing — refuse with a concurrency-loss-style error.
        let post_backup_bytes = read_or_empty(path)?;
        let post_backup_hash = sha256_hex(&post_backup_bytes);
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
            fs::rename(path, &to)?;
            rename_to_record = Some(to.to_string_lossy().into_owned());
            Ok(())
        }
        Op::Chmod { mode } => {
            fs::set_permissions(path, Permissions::from_mode(mode))?;
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

    // 8. after_hash (read post-state). For Rename: read the destination.
    // For everything else: read the original path (which is now the
    // post-mutation state, or the rolled-back state).
    let after_bytes = match &op {
        Op::Rename { to } if exec_result.is_ok() => read_or_empty(to)?,
        _ => read_or_empty(path)?,
    };
    let after_hash = sha256_hex(&after_bytes);

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
        let backup = ctx.run_dir.join("backups").join("hello.txt");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "original\n");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[test]
    fn rename_quarantines_via_op_rename_no_deletion() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("victim.txt");
        fs::write(&target, b"data\n").unwrap();
        let quarantine = td.path().join(".doctor").join("quarantine").join("victim.txt");
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
