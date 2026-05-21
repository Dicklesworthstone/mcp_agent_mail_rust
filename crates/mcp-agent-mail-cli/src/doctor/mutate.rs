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

use super::platform;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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
    /// Execute `sql` against an existing SQLite DB file. The chokepoint
    /// backs up the main DB file before execution; callers that require
    /// WAL/SHM sidecar guarantees must checkpoint or lock externally.
    DbExec { sql: String },
    /// Versioned schema migration marker for an existing SQLite DB file.
    /// Migration SQL is supplied separately through `DbExec`.
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
///
/// The `phase` field distinguishes the two-phase write protocol.
/// `"pending"` is appended before the mutation executes, after the backup
/// is in place. `"completed"` is appended after the mutation succeeds or
/// rolls back. Undo treats a pending-without-completed pair as a
/// crash-window record: the backup exists, so restore from it. Legacy
/// `actions.jsonl` lines without `phase` are treated as completed.
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
    /// `"pending"` (pre-mutation) or `"completed"` (post-mutation).
    /// Absent means legacy / completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
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
    /// Additional flock files to hold for the duration of each mutation.
    /// Fixers can use these to coordinate with subsystem locks such as
    /// `<repo>/.git/am.git-serialize.lock` or a SQLite database lock. Locks
    /// are acquired in declaration order after the per-path lock and
    /// released on return. If any extra lock cannot be acquired
    /// non-blockingly, `mutate()` refuses with `MutateError::LockHeld`.
    pub extra_locks: Vec<PathBuf>,
}

impl MutateContext {
    /// Acquire each `extra_locks` path with `try_lock_exclusive`. Returns
    /// the held file handles; they auto-release when the returned `Vec`
    /// drops. If any fails, returns `Err(LockHeld)` immediately and the
    /// already-acquired locks release as the temporary Vec drops.
    fn acquire_extra_locks(&self) -> Result<Vec<std::fs::File>, MutateError> {
        let mut held = Vec::with_capacity(self.extra_locks.len());
        for path in &self.extra_locks {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let f = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)?;
            if f.try_lock_exclusive().is_err() {
                return Err(MutateError::LockHeld(path.clone()));
            }
            held.push(f);
        }
        Ok(held)
    }
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
    /// The live file's hash changed between step 4 (before_hash) and step 5
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
    /// File-oriented ops intentionally refuse symlink leaves. Otherwise
    /// `fs::copy`/hashing may follow the link while atomic writes replace the
    /// link itself, leaving undo without a faithful backup of the original
    /// filesystem object.
    #[error("path {0} is a symlink; use Op::SymlinkAtomic or refuse the fixer")]
    SymlinkRefused(PathBuf),
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

/// Stream SHA-256 over the file's bytes without loading the entire file into
/// memory. Returns the empty-file hash if the path doesn't exist.
fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn sha256_path_bytes(path: &Path) -> String {
    sha256_bytes(&platform::os_str_bytes(path.as_os_str()))
}

fn sha256_for_path_before_op(path: &Path, op: &Op) -> std::io::Result<String> {
    // For ops that legitimately operate on a symlink WITHOUT
    // dereferencing it — `Op::SymlinkAtomic` (replace a symlink) and
    // `Op::Rename` (quarantine a symlink, moving the link itself) —
    // hash the link's target string rather than trying to read it
    // as a regular file (which O_NOFOLLOW would reject).
    if matches!(op, Op::SymlinkAtomic { .. } | Op::Rename { .. }) {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Ok(sha256_path_bytes(&fs::read_link(path)?));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(sha256_bytes(b""));
            }
            Err(e) => return Err(e),
        }
    }
    sha256_of_path(path)
}

fn sha256_of_path(path: &Path) -> std::io::Result<String> {
    // Directories hash via a deterministic tree digest (used by
    // directory `Op::Rename` quarantine). symlink_metadata avoids
    // following a symlink-to-dir — only a real directory dispatches
    // here; a symlink falls through to the regular-file path (which
    // O_NOFOLLOW rejects, matching the symlink-refused contract).
    if matches!(fs::symlink_metadata(path), Ok(meta) if meta.file_type().is_dir()) {
        return sha256_of_dir_tree(path);
    }
    sha256_of_regular_file(path)
}

fn sha256_of_regular_file(path: &Path) -> std::io::Result<String> {
    let mut f = match open_regular_file_no_follow(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(sha256_bytes(b""));
        }
        Err(e) => return Err(e),
    };
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

/// Digest of a symlink's target string, identical to the
/// chokepoint's `before_hash` for a symlink `Op::Rename`
/// (`sha256_for_path_before_op`). Lets undo verify a quarantined
/// symlink still matches the recorded mutation result before
/// renaming it back — without ever dereferencing the link.
pub(crate) fn sha256_of_symlink_target(path: &Path) -> std::io::Result<String> {
    Ok(sha256_path_bytes(&fs::read_link(path)?))
}

/// Deterministic content+structure digest of a directory tree.
///
/// Used so directory `Op::Rename` (quarantine) is hash-witnessed
/// and undo can verify the quarantined tree still matches the
/// mutation result before moving it back. The digest is:
/// - **order-independent**: entries are sorted by relative path
///   before hashing, so readdir order doesn't affect the result.
/// - **structure + content sensitive**: each entry contributes its
///   relative path, type, mode bits, and either its file-content
///   hash (regular file), symlink target (symlink), or a `dir`
///   marker (directory). A changed byte, mode, added/removed file,
///   or retargeted symlink changes the digest.
///
/// Two callers compute this independently (the chokepoint on the
/// source dir before rename; undo on the quarantined dir before
/// restore) and MUST agree — keep the serialization stable.
pub(crate) fn sha256_of_dir_tree(root: &Path) -> std::io::Result<String> {
    fn walk(base: &Path, cur: &Path, out: &mut Vec<(String, String)>) -> std::io::Result<()> {
        let mut children: Vec<fs::DirEntry> =
            fs::read_dir(cur)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for entry in children {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            let mode = platform::permission_mode(&meta);
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if ft.is_dir() {
                out.push((rel.clone(), format!("dir:{mode:o}")));
                walk(base, &path, out)?;
            } else if ft.is_symlink() {
                let target = fs::read_link(&path)?;
                out.push((
                    rel,
                    format!("symlink:{mode:o}:{}", target.to_string_lossy()),
                ));
            } else if ft.is_file() {
                let content = sha256_of_regular_file(&path)?;
                out.push((rel, format!("file:{mode:o}:{content}")));
            }
            // FIFOs / sockets / devices are intentionally skipped:
            // they don't belong in a mailbox archive and can't be
            // meaningfully content-hashed.
        }
        Ok(())
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for (rel, digest) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn open_regular_file_no_follow(path: &Path) -> std::io::Result<File> {
    // Round-7 Gemini F2 (P1): O_NONBLOCK mirrors the undo helper —
    // defeats the FIFO-blocks-open DoS in the chokepoint's hash/backup
    // paths. No-op for regular files. The Unix `O_NOFOLLOW | O_NONBLOCK`
    // open + post-open regular-file check (and the Windows
    // reparse-point-refusal equivalent) live in `platform`.
    platform::open_regular_file_no_follow(path)
}

/// The recorded `before_mode` value for the action log.
///
/// Unix: the *raw* `Metadata::mode()` (file-type bits included, NOT masked
/// to `0o7777`) — `before_mode`/`after_mode` are recorded and round-tripped
/// verbatim, and undo's `restore_mode` feeds it back through
/// `from_mode`/`set_permission_mode` (which ignores the high bits). Keeping
/// the raw value preserves the existing action-log contract byte-for-byte.
///
/// Windows: synthesize from the read-only attribute, matching
/// `platform::permission_mode`.
#[cfg(unix)]
fn recorded_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn recorded_mode(meta: &std::fs::Metadata) -> u32 {
    platform::permission_mode(meta)
}

fn read_or_empty(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = match open_regular_file_no_follow(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Canonicalize `path` resolving symlinks, or fall back to canonicalizing
/// the nearest existing ancestor and joining the file name. This avoids
/// "no such file or directory" when the path is a target we're about to
/// CREATE.
pub(crate) fn canonicalize_existing_or_parent(path: &Path) -> std::io::Result<PathBuf> {
    // If the FINAL component is a symlink, canonicalize its PARENT
    // and append the link's own name — NEVER follow the link. Scope
    // checks must anchor to where the link SITS, not where it
    // points. Without this, quarantining an unexpected symlink whose
    // target is out of scope (e.g. an archive entry aliasing
    // `/etc/shadow`) would canonicalize to the target and be
    // false-refused as `OutOfScope` — defeating the very fix. (Uses
    // `symlink_metadata`, which does not traverse the link.)
    if matches!(fs::symlink_metadata(path), Ok(meta) if meta.file_type().is_symlink()) {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let parent_canon = canonicalize_existing_or_parent(parent)?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "symlink path has no file name",
            )
        })?;
        return Ok(parent_canon.join(name));
    }
    if path.exists() {
        return path.canonicalize();
    }
    // Round-7 Gemini F4 (P2): walk up to the first existing
    // ancestor while accumulating ALL missing intermediate
    // components. Pre-round-7 only kept `file_name()` and threw
    // every intermediate dir away, so `scope/a/b/file.txt`
    // (where `scope/a` is missing) canonicalized to
    // `<canon scope>/file.txt` — and `ensure_in_scope` then
    // false-rejected paths whose actual write_scope was
    // `scope/a/...`.
    let mut cur = path.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        let name = cur.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no existing ancestor for path",
            )
        })?;
        missing.push(name.to_os_string());
        cur = match cur.parent() {
            // Round-8 Gemini F2 (P2): bare relative paths like
            // `Path::new("file.txt")` have an empty-string parent
            // which doesn't exist on disk and has no file_name —
            // the loop would error on the second iteration even
            // though the intent is "resolve relative to cwd".
            // Map empty path → `.` so the cwd canonicalizes.
            Some(p) if p.as_os_str().is_empty() => PathBuf::from("."),
            Some(p) => p.to_path_buf(),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no existing ancestor for path",
                ));
            }
        };
    }
    let mut result = cur.canonicalize()?;
    // `missing` was pushed leaf-first; reverse for top-down order.
    for name in missing.iter().rev() {
        result.push(name);
    }
    Ok(result)
}

pub(crate) fn ensure_in_scope(caps: &Capabilities, path: &Path) -> Result<(), MutateError> {
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

fn reject_unexpected_symlink(path: &Path, op: &Op) -> Result<(), MutateError> {
    // `Op::SymlinkAtomic` creates/replaces a symlink; `Op::Rename`
    // MOVES the path (quarantine) via `fs::rename`, which operates
    // on the link itself and never dereferences it. Both are safe
    // on a symlink source — no write is ever directed THROUGH the
    // link to its target. Every other op (WriteFile / AppendFile /
    // Chmod / DbExec) would follow the link and write to an
    // out-of-scope target, so they stay refused.
    if matches!(op, Op::SymlinkAtomic { .. } | Op::Rename { .. }) {
        return Ok(());
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(MutateError::SymlinkRefused(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MutateError::Io(e)),
    }
}

fn ensure_existing_regular_db_file(path: &Path, op: &'static str) -> Result<(), MutateError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(()),
        Ok(_) => Err(MutateError::ExecFailed {
            path: path.to_path_buf(),
            op,
            message: "database path is not a regular file".to_string(),
            rolled_back: None,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(MutateError::ExecFailed {
            path: path.to_path_buf(),
            op,
            message: "database file does not exist; DB mutations require a pre-existing file for reversible backup".to_string(),
            rolled_back: None,
        }),
        Err(e) => Err(MutateError::Io(e)),
    }
}

fn copy_verbatim_with_perms(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut src_file = open_regular_file_no_follow(src)?;
    let meta = src_file.metadata()?;
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut dst_file = tmp.as_file();
        std::io::copy(&mut src_file, &mut dst_file)?;
        dst_file.sync_data()?;
        platform::set_permission_mode(dst_file, platform::permission_mode(&meta))?;
    }
    tmp.persist(dst).map_err(|e| e.error)?;
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|d| d.sync_all());
    Ok(())
}

fn copy_symlink_target(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = fs::read_link(src)?;
    atomic_write_file(dst, &platform::os_str_bytes(target.as_os_str()), 0o600)
}

fn cmp_symlink_target(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = fs::read_link(src)?;
    let backup = read_or_empty(dst)?;
    if platform::os_str_bytes(target.as_os_str()) == backup {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "backup verify failed (symlink target mismatch)",
        ))
    }
}

/// Streaming comparison of two files. Reads 64 KiB at a time and aborts on
/// the first divergence.
fn cmp_strict(a: &Path, b: &Path) -> std::io::Result<()> {
    let mut fa = open_regular_file_no_follow(a)?;
    let mut fb = open_regular_file_no_follow(b)?;
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
/// Permissions are set on the tempfile's file descriptor before
/// `tmp.persist(path)`. Setting permissions on `path` after persist would
/// race a symlink-swap attacker who could redirect `path` to an arbitrary
/// out-of-scope file between persist and chmod.
pub(crate) fn atomic_write_file(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut f = tmp.as_file();
        f.write_all(content)?;
        f.sync_data()?;
        // Chmod via fd before persist so a path swap cannot redirect it.
        platform::set_permission_mode(f, mode)?;
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
/// When `path` is absolute and outside `repo_root`, `strip_prefix` returns
/// Err and `PathBuf::join(absolute)` drops the base, so a naive
/// `run_dir.join("backups").join(rel)` would return the live target itself.
/// Out-of-repo paths are encoded under a sentinel `__abs__/` subdirectory.
///
/// Backups are scoped under `backups/<started_at_ns>/` so two mutations to
/// the same path in one run cannot overwrite each other's backups. Undo
/// finds each backup via the recorded `started_at_ns`.
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
pub(crate) fn atomic_symlink(path: &Path, target: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if let Ok(meta) = fs::symlink_metadata(path)
        && !meta.file_type().is_symlink()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-symlink path {}", path.display()),
        ));
    }
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace pre-existing temporary symlink path {}",
                tmp_path.display()
            ),
        ));
    }
    platform::create_symlink(target, &tmp_path)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Append `content` to `path` (creating if missing).
///
/// Uses `O_NOFOLLOW` so a symlink-swap attacker who replaces `path` with a
/// symlink between the hash check and this open cannot redirect the append to
/// an out-of-scope file. On a symlink, open returns `ELOOP`, which maps to
/// `MutateError::Io`.
#[cfg(unix)]
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

/// Windows equivalent of the Unix `O_NOFOLLOW` append: open the link itself
/// (refusing reparse points) so a symlink/junction-swap attacker cannot
/// redirect the append to an out-of-scope file, then append.
#[cfg(not(unix))]
fn append_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // FILE_FLAG_OPEN_REPARSE_POINT opens the link itself instead of
    // following it; we then reject any reparse point (symlink / junction /
    // mount point), mirroring the Unix O_NOFOLLOW append.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if f.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a reparse point (symlink/junction); refusing append",
                path.display()
            ),
        ));
    }
    f.write_all(content)?;
    f.sync_data()?;
    Ok(())
}

/// `O_NOFOLLOW` value as a const so we don't need a libc dep.
///
/// Only the Unix `append_file`/`chmod_via_fd` paths reference these; the
/// Windows equivalents use the reparse-point flags inline. The shared
/// `open_regular_file_no_follow` (hash/backup paths, where the FIFO-blocks-
/// open DoS also mattered and `O_NONBLOCK` was needed) now lives in
/// `super::platform`, so `O_NONBLOCK` is no longer referenced here.
#[cfg(unix)]
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

/// Set permissions on `path` via the file descriptor.
///
/// Opens the file with `O_NOFOLLOW` first so a symlink-swap attacker
/// cannot redirect the chmod to an arbitrary file. Returns `ELOOP` if
/// `path` is now a symlink.
#[cfg(unix)]
fn chmod_via_fd(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc_consts::O_NOFOLLOW)
        .open(path)?;
    platform::set_permission_mode(&f, mode)?;
    Ok(())
}

/// Windows equivalent of the Unix `O_NOFOLLOW` chmod: open the link itself
/// (refusing reparse points) so a symlink/junction-swap attacker cannot
/// redirect the chmod, then map the mode onto the read-only attribute.
#[cfg(not(unix))]
fn chmod_via_fd(path: &Path, mode: u32) -> std::io::Result<()> {
    let f = platform::open_regular_file_no_follow(path)?;
    platform::set_permission_mode(&f, mode)?;
    Ok(())
}

/// THE chokepoint. Every fixer-driven mutation flows through this.
///
/// Steps in order:
/// 1. Validate preconditions (path in scope, rename destination in scope).
/// 2. For dry-run, compute `before_hash` and return without write artifacts.
/// 3. Per-path advisory lock (`fs2::FileExt::try_lock_exclusive`).
/// 4. Compute `before_hash`.
/// 5. Write verbatim backup; verify with `cmp_strict`; verify
///    `sha256(live) == before_hash` (TOCTOU defense; if mismatch, refuse).
/// 6. Plan the mutation in memory.
/// 7. Execute atomically.
/// 8. On exec failure: ATOMIC rollback from backup; record truthful
///    `rolled_back` value.
/// 9. Compute `after_hash`.
/// 10. Append to `actions.jsonl`; fsync; release lock.
///
/// Errors:
/// - `Err(MutateError::ExecFailed(_))` if the mutation could not be
///   completed. Callers using `?` see a real error rather than
///   `Ok(ActionResult { ok: false })`.
/// - `Err(MutateError::TamperedBeforeMutate)` if the live file's hash no
///   longer matches the backup's right after we copied it (TOCTOU
///   detected).
pub fn mutate(ctx: &MutateContext, path: &Path, op: Op) -> Result<ActionResult, MutateError> {
    let started_at_ns = elapsed_ns(ctx.start);

    // 1. Preconditions: path in scope + Rename destination in scope. This
    // must precede lock scaffolding so refused paths and dry-runs do not
    // create out-of-scope parent directories or .doctor-lock files.
    ensure_in_scope(&ctx.capabilities, path)?;
    if let Op::Rename { to } = &op {
        ensure_in_scope(&ctx.capabilities, to)?;
    }
    reject_unexpected_symlink(path, &op)?;
    if matches!(op, Op::DbExec { .. } | Op::DbMigrate { .. }) {
        ensure_existing_regular_db_file(path, op.op_kind())?;
    }

    if ctx.dry_run {
        let before_hash = sha256_for_path_before_op(path, &op)?;
        eprintln!(
            "[dry-run] would mutate {} via {}",
            path.display(),
            op.op_kind()
        );
        return Ok(ActionResult {
            ok: true,
            before_hash: before_hash.clone(),
            after_hash: before_hash,
            error: None,
        });
    }

    // 3. Per-path advisory lock. Lock file lives next to target, distinct name.
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

    // Hold any subsystem locks declared by the fixer for the whole mutation.
    // If any extra lock fails, release the per-path lock before returning.
    let _extra_lock_guards = match ctx.acquire_extra_locks() {
        Ok(g) => g,
        Err(e) => {
            let _ = FileExt::unlock(&lock_file);
            return Err(e);
        }
    };

    // 4. before_hash + before_mode. Stream-hash the file rather than
    // reading entire contents into memory.
    let before_hash = sha256_for_path_before_op(path, &op)?;
    let before_mode = if matches!(op, Op::SymlinkAtomic { .. })
        && matches!(
            fs::symlink_metadata(path),
            Ok(meta) if meta.file_type().is_symlink()
        ) {
        None
    } else {
        fs::metadata(path).ok().map(|m| recorded_mode(&m))
    };

    // 5. Verbatim backup (only if file exists). Also re-verifies that the
    // live file still hashes to before_hash after copying. If not, a
    // concurrent writer modified the file in our window and the backup would
    // not represent the true pre-mutation state.
    //
    // `backup_path_for` encodes absolute paths outside repo_root without
    // letting PathBuf::join drop the backup prefix. The `rel` value recorded
    // in actions.jsonl preserves the original path semantics for undo.
    let backup_path = backup_path_for(&ctx.run_dir, &ctx.repo_root, path, started_at_ns);
    let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
    let path_meta_kind = fs::symlink_metadata(path);
    let path_is_dir = matches!(&path_meta_kind, Ok(meta) if meta.file_type().is_dir());
    let path_is_symlink = matches!(&path_meta_kind, Ok(meta) if meta.file_type().is_symlink());
    if !ctx.dry_run && fs::symlink_metadata(path).is_ok() {
        if path_is_dir || (path_is_symlink && matches!(op, Op::Rename { .. })) {
            // Non-regular-file quarantine via `Op::Rename`: a
            // directory tree OR a symlink. `fs::rename` preserves
            // the tree / the link itself intact at the destination
            // `to` — that IS the backup, and undo reverses by
            // renaming it back. We skip the verbatim file-copy
            // (a recursive dir copy would be redundant + risk
            // partial-copy failures; a symlink isn't a regular file
            // so the copy path can't read it anyway). A symlink is
            // moved as the link — never dereferenced.
            //
            // For a directory, `Op::Rename` is the ONLY supported
            // op (every other op would need to follow into the
            // tree); reject anything else.
            if path_is_dir && !matches!(op, Op::Rename { .. }) {
                let _ = FileExt::unlock(&lock_file);
                return Err(MutateError::Unsupported(
                    "only Op::Rename is supported for directory paths",
                ));
            }
            // Still enforce the tamper defense: re-hash (dir-tree
            // digest, or the symlink target string) and refuse if a
            // concurrent writer changed it in our window.
            let post_backup_hash = sha256_for_path_before_op(path, &op)?;
            if post_backup_hash != before_hash {
                let _ = FileExt::unlock(&lock_file);
                return Err(MutateError::TamperedBeforeMutate(path.to_path_buf()));
            }
        } else {
            if matches!(op, Op::SymlinkAtomic { .. })
                && matches!(
                    fs::symlink_metadata(path),
                    Ok(meta) if meta.file_type().is_symlink()
                )
            {
                copy_symlink_target(path, &backup_path)?;
                cmp_symlink_target(path, &backup_path)
                    .map_err(|_| MutateError::BackupVerify(path.to_path_buf()))?;
            } else {
                copy_verbatim_with_perms(path, &backup_path)?;
                cmp_strict(path, &backup_path)
                    .map_err(|_| MutateError::BackupVerify(path.to_path_buf()))?;
            }
            // Re-hash the live file; if it changed since step 4, someone else
            // is writing, so refuse to proceed.
            let post_backup_hash = sha256_for_path_before_op(path, &op)?;
            if post_backup_hash != before_hash {
                let _ = FileExt::unlock(&lock_file);
                return Err(MutateError::TamperedBeforeMutate(path.to_path_buf()));
            }
        }
    }

    let mut rename_to_record: Option<String> = None;
    let mut after_mode: Option<u32> = None;

    // Write a pending action before the mutation. If the process dies
    // mid-mutation, undo can pair the pending line with the verbatim backup
    // and restore without needing a completed action.
    {
        let pending_record = ActionRecord {
            path: rel.to_string_lossy().into_owned(),
            op: op.op_kind().to_string(),
            before_hash: before_hash.clone(),
            after_hash: String::new(), // unknown until step 9
            started_at_ns,
            finished_at_ns: 0, // not yet finished
            run_id: ctx.run_id.clone(),
            fixer_id: ctx.fixer_id.clone(),
            ok: false, // mutation hasn't executed yet
            phase: Some("pending"),
            rename_to: match &op {
                Op::Rename { to } => Some(to.to_string_lossy().into_owned()),
                _ => None,
            },
            before_mode,
            after_mode: None,
            error: None,
            rolled_back: None,
        };
        let pending_line = serde_json::to_string(&pending_record)? + "\n";
        let mut f = ctx
            .actions_file
            .lock()
            .map_err(|_| MutateError::Unsupported("actions_file mutex poisoned"))?;
        f.write_all(pending_line.as_bytes())?;
        f.sync_data()?;
    }

    // 7. Execute atomically.
    let exec_result: Result<(), MutateError> = match op.clone() {
        Op::WriteFile { content, mode } => {
            match atomic_write_file(path, &content, mode).map_err(MutateError::Io) {
                Ok(()) => {
                    after_mode = Some(mode);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Op::AppendFile { content } => append_file(path, &content).map_err(MutateError::Io),
        Op::Rename { to } => {
            let result = (|| -> Result<(), MutateError> {
                // Destination scope already checked at step 1.
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(MutateError::Io)?;
                }
                // Also acquire an advisory lock on the destination basename.
                // The source lock protects `path`; the destination lock
                // prevents two concurrent renames from racing toward the same
                // target.
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
                    .open(&to_lock_path)
                    .map_err(MutateError::Io)?;
                if to_lock_file.try_lock_exclusive().is_err() {
                    return Err(MutateError::LockHeld(to.clone()));
                }
                // Refuse if the destination already exists. POSIX
                // `fs::rename` overwrites silently, which would destroy the
                // existing file at `to`. Check after acquiring the destination
                // lock so concurrent renames to the same target cannot race
                // past it.
                if fs::symlink_metadata(&to).is_ok() {
                    let _ = FileExt::unlock(&to_lock_file);
                    return Err(MutateError::RenameDestinationExists(to.clone()));
                }
                fs::rename(path, &to).map_err(MutateError::Io)?;
                let _ = FileExt::unlock(&to_lock_file);
                Ok(())
            })();
            if result.is_ok() {
                rename_to_record = Some(to.to_string_lossy().into_owned());
            }
            result
        }
        // Chmod via fd opened with O_NOFOLLOW so a symlink-swap attacker
        // cannot redirect to an out-of-scope file.
        Op::Chmod { mode } => match chmod_via_fd(path, mode).map_err(MutateError::Io) {
            Ok(()) => {
                after_mode = Some(mode);
                Ok(())
            }
            Err(e) => Err(e),
        },
        Op::DbExec { sql } => {
            // Open the DB at `path`, run the SQL via `execute_raw`
            // (handles DDL like PRAGMA + CREATE), then close.
            // The chokepoint already byte-copied `path` to
            // `backup_path` earlier, so the rollback path (on
            // exec failure) restores the file.
            //
            // WAL/SHM caveat: SQLite may write `<path>-wal` and
            // `<path>-shm` siblings during exec, and these are NOT
            // backed up. The file-level rollback restores `<path>`
            // byte-identical but the siblings persist. SQLite is
            // robust to stale WAL/SHM on the next open, so the
            // operational impact is bounded. Callers that need
            // stronger guarantees should `PRAGMA wal_checkpoint(TRUNCATE);`
            // before invoking and ensure no other writers can race.
            //
            // **Connection scope:** the SqliteConnection is bound
            // inside the match expression — when this arm returns
            // (either Ok or Err) the binding drops, which closes the
            // connection cleanly before the outer rollback runs.
            use sqlmodel_sqlite::SqliteConnection;
            match SqliteConnection::open_file(path.to_string_lossy().into_owned()) {
                Ok(conn) => match conn.execute_raw(&sql) {
                    Ok(()) => {
                        drop(conn);
                        Ok(())
                    }
                    Err(e) => Err(MutateError::ExecFailed {
                        path: path.to_path_buf(),
                        op: "DbExec",
                        message: format!("execute_raw failed: {e}"),
                        rolled_back: None,
                    }),
                },
                Err(e) => Err(MutateError::ExecFailed {
                    path: path.to_path_buf(),
                    op: "DbExec",
                    message: format!("open_file failed: {e}"),
                    rolled_back: None,
                }),
            }
        }
        Op::DbMigrate { from, to } => {
            // DbMigrate is a marker op: it records the migration
            // intent (from → to) in actions.jsonl with file-level
            // backup, but the actual migration SQL must be supplied
            // via separate Op::DbExec calls. This keeps the chokepoint
            // simple: every SQL fragment is hash-witnessed independently,
            // and undo can replay in reverse.
            //
            // Records the (from, to) values in the trailing
            // actions.jsonl entry so undo can detect partial
            // migrations. No SQL is run here — the bookkeeping IS
            // the operation.
            let _ = (from, to); // captured into the action record below
            Ok(())
        }
        Op::SymlinkAtomic { target } => atomic_symlink(path, &target).map_err(MutateError::Io),
    };

    // 8. On exec failure: attempt atomic rollback and record the actual
    // `rolled_back` outcome.
    let rolled_back: Option<bool> = if exec_result.is_err() {
        if matches!(op, Op::SymlinkAtomic { .. }) {
            None
        } else if backup_path.exists() && path.exists() {
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

    // 9. after_hash. Prefer op-derived hashes when the successful mutation
    // deterministically preserves or replaces bytes; fall back to reading the
    // live path for append/failure cases.
    let after_hash = match &op {
        Op::WriteFile { content, .. } if exec_result.is_ok() => sha256_bytes(content),
        Op::Rename { .. } if exec_result.is_ok() => before_hash.clone(),
        Op::Chmod { .. } if exec_result.is_ok() => before_hash.clone(),
        Op::SymlinkAtomic { target } if exec_result.is_ok() => sha256_path_bytes(target),
        Op::SymlinkAtomic { .. } => before_hash.clone(),
        _ => sha256_of_path(path)?,
    };

    // 10. Append to actions.jsonl, fsync. The `rolled_back` field reflects
    // the actual rollback result, not an assumption.
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
        // This post-mutation entry pairs with the earlier `pending` line via
        // the shared `started_at_ns`.
        phase: Some("completed"),
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

    // Return Err on exec failure, not Ok with ok: false.
    if let Err(e) = exec_result {
        if matches!(&e, MutateError::RenameDestinationExists(_)) {
            return Err(e);
        }
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
    // The chokepoint's production code no longer imports these at module
    // scope (Windows has no POSIX modes), but the tests exercise Unix mode
    // semantics directly and run only on the host. The test module is never
    // compiled for Windows (`cargo check` skips `#[cfg(test)]`), so these
    // Unix-only imports are safe here.
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

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
            extra_locks: Vec::new(),
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
    fn atomic_symlink_refuses_regular_destination_without_clobbering() {
        let td = TempDir::new().unwrap();
        let latest = td.path().join("latest");
        fs::write(&latest, "operator data\n").unwrap();

        let err = atomic_symlink(&latest, Path::new("runs/next"))
            .expect_err("regular destination must be preserved");

        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&latest).unwrap(), "operator data\n");
    }

    #[test]
    fn symlink_atomic_hashes_link_target_without_following_it() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-12T12-00-00Z__symlink");
        let latest = td.path().join("latest");
        let target = PathBuf::from("runs/missing-future-run");

        let result = mutate(
            &ctx,
            &latest,
            Op::SymlinkAtomic {
                target: target.clone(),
            },
        )
        .unwrap();

        assert!(result.ok);
        assert_eq!(fs::read_link(&latest).unwrap(), target);
        assert_eq!(result.after_hash, sha256_path_bytes(&target));

        let actions = fs::read_to_string(ctx.run_dir.join("actions.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = actions
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "symlink mutation must be completed");
        assert_eq!(lines[1]["phase"], "completed");
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["after_hash"], sha256_path_bytes(&target));
    }

    #[test]
    fn write_file_refuses_symlink_leaf_without_clobbering() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-12T14-00-00Z__write-symlink");
        let real = td.path().join("real-config.json");
        let link = td.path().join("config.json");
        fs::write(&real, "{\"version\":1}\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = mutate(
            &ctx,
            &link,
            Op::WriteFile {
                content: b"{\"version\":2}\n".to_vec(),
                mode: 0o600,
            },
        )
        .expect_err("WriteFile must not replace a symlink leaf");

        assert!(matches!(err, MutateError::SymlinkRefused(p) if p == link));
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(fs::read_to_string(&link).unwrap(), "{\"version\":1}\n");
        assert_eq!(
            fs::read_to_string(ctx.run_dir.join("actions.jsonl")).unwrap(),
            "",
            "refusal must happen before pending action logging"
        );
    }

    #[test]
    fn append_file_refuses_symlink_leaf_without_rollback_clobbering() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-12T14-01-00Z__append-symlink");
        let real = td.path().join("real.log");
        let link = td.path().join("log.txt");
        fs::write(&real, "base\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = mutate(
            &ctx,
            &link,
            Op::AppendFile {
                content: b"doctor\n".to_vec(),
            },
        )
        .expect_err("AppendFile must not enter rollback on a symlink leaf");

        assert!(matches!(err, MutateError::SymlinkRefused(p) if p == link));
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(fs::read_to_string(&real).unwrap(), "base\n");
        assert_eq!(
            fs::read_to_string(ctx.run_dir.join("actions.jsonl")).unwrap(),
            "",
            "refusal must happen before backup or action logging"
        );
    }

    #[test]
    fn write_file_refuses_dangling_symlink_leaf_without_clobbering() {
        let td = TempDir::new().unwrap();
        let ctx = make_ctx(&td, "2026-05-12T14-02-00Z__dangling-symlink");
        let missing_target = td.path().join("missing-target");
        let link = td.path().join("config.json");
        std::os::unix::fs::symlink(&missing_target, &link).unwrap();

        let err = mutate(
            &ctx,
            &link,
            Op::WriteFile {
                content: b"replacement\n".to_vec(),
                mode: 0o644,
            },
        )
        .expect_err("dangling symlink leaves must still be refused");

        assert!(matches!(err, MutateError::SymlinkRefused(p) if p == link));
        assert_eq!(fs::read_link(&link).unwrap(), missing_target);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn regular_file_helpers_refuse_symlink_leafs_without_following() {
        let td = TempDir::new().unwrap();
        let sensitive = td.path().join("sensitive.txt");
        let link = td.path().join("config.json");
        let backup = td.path().join("backup.json");
        fs::write(&sensitive, "secret\n").unwrap();
        std::os::unix::fs::symlink(&sensitive, &link).unwrap();

        assert!(
            sha256_of_path(&link).is_err(),
            "hashing regular-file content must not follow a symlink leaf"
        );
        assert!(
            read_or_empty(&link).is_err(),
            "backup reads must not follow a symlink leaf"
        );
        assert!(
            copy_verbatim_with_perms(&link, &backup).is_err(),
            "backup copy must not follow a symlink leaf"
        );
        assert!(
            fs::symlink_metadata(&backup).is_err(),
            "failed symlink backup must not create an artifact"
        );
        assert_eq!(fs::read_to_string(&sensitive).unwrap(), "secret\n");
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
        // actions.jsonl contains two lines per mutation: pending, then
        // completed. Validate the completed action record.
        let lines: Vec<serde_json::Value> = actions
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // First line should be pending, second completed.
        assert_eq!(lines.len(), 2, "expected 2 lines (pending + completed)");
        assert_eq!(lines[0]["phase"], "pending");
        let v = &lines[1];
        assert_eq!(v["phase"], "completed");
        assert_eq!(v["op"], "WriteFile");
        assert!(v["before_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(v["after_hash"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn write_file_backs_up_existing_content_verbatim() {
        // Backups live under `backups/seq_<started_at_ns>/<rel>` so multiple
        // mutations to the same file in one run cannot overwrite each
        // other's backups.
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
    fn multiple_mutations_to_same_path_get_distinct_backups() {
        // Two mutations to the same path must not overwrite each other's
        // backups. Each mutation gets its own seq_<ns> subdir.
        let td = TempDir::new().unwrap();
        let target = td.path().join("collide.txt");
        fs::write(&target, b"v0\n").unwrap();
        let ctx = make_ctx(&td, "2026-05-09T16-30-15Z__multi");
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
    fn dir_rename_quarantines_whole_tree_via_op_rename() {
        let td = TempDir::new().unwrap();
        // Plant a debris directory tree with nested files.
        let debris = td.path().join("debris-bundle");
        fs::create_dir_all(debris.join("nested")).unwrap();
        fs::write(debris.join("manifest.json"), b"{\"partial\":true}").unwrap();
        fs::write(debris.join("nested").join("payload.bin"), b"\x00\x01\x02").unwrap();

        let quarantine = td
            .path()
            .join(".doctor")
            .join("quarantine")
            .join("debris-bundle");
        let ctx = make_ctx(&td, "2026-05-20T00-00-00Z__dir");
        let result = mutate(
            &ctx,
            &debris,
            Op::Rename {
                to: quarantine.clone(),
            },
        )
        .unwrap();
        assert!(result.ok, "dir rename should succeed");
        assert!(!debris.exists(), "source dir removed by rename");
        assert!(quarantine.is_dir(), "destination is the moved dir tree");
        assert_eq!(
            fs::read(quarantine.join("nested").join("payload.bin")).unwrap(),
            b"\x00\x01\x02",
            "nested file content preserved by rename"
        );
        // before_hash == after_hash for a rename, and both are the
        // deterministic dir-tree digest (non-empty).
        assert!(result.before_hash.starts_with("sha256:"));
        assert_eq!(result.before_hash, result.after_hash);
    }

    #[test]
    fn dir_tree_hash_is_order_independent_and_content_sensitive() {
        let td = TempDir::new().unwrap();
        let a = td.path().join("a");
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::write(a.join("z.txt"), b"zz").unwrap();
        fs::write(a.join("sub").join("y.txt"), b"yy").unwrap();
        let h1 = sha256_of_dir_tree(&a).unwrap();
        // Identical tree built in a different creation order → same hash.
        let b = td.path().join("b");
        fs::create_dir_all(b.join("sub")).unwrap();
        fs::write(b.join("sub").join("y.txt"), b"yy").unwrap();
        fs::write(b.join("z.txt"), b"zz").unwrap();
        let h2 = sha256_of_dir_tree(&b).unwrap();
        assert_eq!(h1, h2, "tree hash must be order-independent");
        // Change one byte → different hash.
        fs::write(b.join("z.txt"), b"zX").unwrap();
        let h3 = sha256_of_dir_tree(&b).unwrap();
        assert_ne!(h1, h3, "tree hash must be content-sensitive");
    }

    #[test]
    fn non_rename_op_on_directory_is_unsupported() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("somedir");
        fs::create_dir_all(&dir).unwrap();
        let ctx = make_ctx(&td, "2026-05-20T00-00-00Z__dirchmod");
        let err = mutate(&ctx, &dir, Op::Chmod { mode: 0o700 }).unwrap_err();
        assert!(
            matches!(err, MutateError::Unsupported(_)),
            "Chmod on a directory must be rejected as Unsupported, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_rename_quarantines_link_without_dereferencing() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        // The symlink points at a "sensitive" target OUTSIDE the
        // archive. Quarantine must move the LINK without ever
        // reading the target (no dereference / exfil).
        let secret = td.path().join("secret.txt");
        fs::write(&secret, b"do-not-read-me").unwrap();
        let link = td.path().join("sneaky.md");
        symlink(&secret, &link).unwrap();

        let quarantine = td
            .path()
            .join(".doctor")
            .join("quarantine")
            .join("sneaky.md");
        let ctx = make_ctx(&td, "2026-05-20T00-00-00Z__symq");
        let result = mutate(
            &ctx,
            &link,
            Op::Rename {
                to: quarantine.clone(),
            },
        )
        .unwrap();
        assert!(result.ok, "symlink rename should succeed");
        // Source link gone; quarantine holds the symlink (still a
        // symlink, still pointing at the original target — NOT a
        // copy of the target's bytes).
        assert!(fs::symlink_metadata(&link).is_err(), "source link moved");
        let q_meta = fs::symlink_metadata(&quarantine).unwrap();
        assert!(q_meta.file_type().is_symlink(), "quarantined as a symlink");
        assert_eq!(
            fs::read_link(&quarantine).unwrap(),
            secret,
            "target preserved"
        );
        // The secret target was never touched.
        assert_eq!(fs::read(&secret).unwrap(), b"do-not-read-me");
        // before==after hash (the target-string digest), non-empty.
        assert!(result.before_hash.starts_with("sha256:"));
        assert_eq!(result.before_hash, result.after_hash);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_rename_round_trips_via_undo() {
        use crate::doctor::undo::run_undo_with_scopes;
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let target = td.path().join("target.bin");
        fs::write(&target, b"x").unwrap();
        let link = td.path().join("link.md");
        symlink(&target, &link).unwrap();
        let original_target = fs::read_link(&link).unwrap();

        let run_id = "2026-05-20T00-00-00Z__symq_rt";
        let quarantine = td.path().join(".doctor").join("quarantine").join("link.md");
        let ctx = make_ctx(&td, run_id);
        mutate(&ctx, &link, Op::Rename { to: quarantine }).unwrap();
        assert!(fs::symlink_metadata(&link).is_err(), "link quarantined");

        let summary =
            run_undo_with_scopes(td.path(), run_id, false, true, &[td.path().to_path_buf()])
                .unwrap();
        assert!(
            summary.failures.is_empty(),
            "undo failures: {:?}",
            summary.failures
        );
        // Link restored, still a symlink, same target.
        let restored = fs::symlink_metadata(&link).unwrap();
        assert!(restored.file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), original_target);
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
        assert!(
            !td.path().join(".hello.txt.doctor-lock").exists(),
            "dry-run must not create advisory lock files"
        );
    }

    #[test]
    fn dry_run_does_not_create_missing_parent_or_lock() {
        let td = TempDir::new().unwrap();
        let mut ctx = make_ctx(&td, "2026-05-09T16-30-15Z__dry");
        ctx.dry_run = true;
        let parent = td.path().join("nested");
        let target = parent.join("hello.txt");
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
        assert!(
            !parent.exists(),
            "dry-run must not create missing parent directories"
        );
        assert!(
            !parent.join(".hello.txt.doctor-lock").exists(),
            "dry-run must not create advisory lock files"
        );
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
    fn out_of_scope_write_refuses_before_lock_artifacts() {
        let scope = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let ctx = make_ctx(&scope, "2026-05-09T16-30-15Z__scope");
        let out_of_scope_parent = outside.path().join("nested");
        let target = out_of_scope_parent.join("target.txt");
        let lock_path = out_of_scope_parent.join(".target.txt.doctor-lock");

        let r = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"x".to_vec(),
                mode: 0o644,
            },
        );

        assert!(matches!(r, Err(MutateError::OutOfScope(_))));
        assert!(
            !out_of_scope_parent.exists(),
            "out-of-scope refusal must not create parent directories"
        );
        assert!(
            !lock_path.exists(),
            "out-of-scope refusal must not create advisory lock files"
        );
        let actions = fs::read_to_string(ctx.run_dir.join("actions.jsonl")).unwrap();
        assert!(
            actions.is_empty(),
            "refused out-of-scope mutations must not append action records"
        );
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
        // The pending line is first; validate the completed line.
        let lines: Vec<serde_json::Value> = actions
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let v = lines
            .iter()
            .find(|l| l["phase"] == "completed")
            .expect("completed line");
        assert_eq!(v["op"], "Chmod");
        // before_mode was 0o644 = 33188 (decimal), after_mode 0o600 = 33152
        assert_eq!(v["after_mode"].as_u64(), Some(0o600));
    }

    #[test]
    fn db_exec_executes_sql_and_records_completed_action() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-09T16-30-15Z__dbexec";
        let ctx = make_ctx(&td, run_id);
        let target = td.path().join("storage.sqlite3");
        {
            let conn =
                sqlmodel_sqlite::SqliteConnection::open_file(target.to_string_lossy().into_owned())
                    .unwrap();
            conn.execute_raw("CREATE TABLE doctor_preexisting (id INTEGER PRIMARY KEY);")
                .unwrap();
        }
        let result = mutate(
            &ctx,
            &target,
            Op::DbExec {
                sql: "\
                    CREATE TABLE doctor_mutate_smoke (id INTEGER PRIMARY KEY, label TEXT NOT NULL);\
                    INSERT INTO doctor_mutate_smoke (id, label) VALUES (1, 'ok');\
                "
                .into(),
            },
        )
        .unwrap();
        assert!(result.ok);

        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(target.to_string_lossy().into_owned())
                .unwrap();
        let rows = conn
            .query_sync("SELECT label FROM doctor_mutate_smoke WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<String>("label").unwrap(), "ok");

        let actions = fs::read_to_string(
            td.path()
                .join(".doctor")
                .join("runs")
                .join(run_id)
                .join("actions.jsonl"),
        )
        .unwrap();
        let lines: Vec<serde_json::Value> = actions
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "successful exec must be hash-witnessed");
        assert_eq!(lines[0]["phase"], "pending");
        assert_eq!(lines[1]["phase"], "completed");
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["op"], "DbExec");
        assert!(lines[1]["error"].is_null());
        assert_ne!(
            lines[1]["before_hash"].as_str(),
            Some(sha256_bytes(b"").as_str()),
            "DbExec must run against a pre-existing DB so undo has a real backup"
        );
    }

    #[test]
    fn db_exec_refuses_missing_database_without_action_record() {
        let td = TempDir::new().unwrap();
        let run_id = "2026-05-09T16-30-15Z__dbexec_missing";
        let ctx = make_ctx(&td, run_id);
        let target = td.path().join("missing.sqlite3");
        let err = mutate(
            &ctx,
            &target,
            Op::DbExec {
                sql: "CREATE TABLE should_not_exist (id INTEGER);".into(),
            },
        )
        .unwrap_err();
        match err {
            MutateError::ExecFailed { message, .. } => {
                assert!(
                    message.contains("does not exist"),
                    "missing DB refusal should explain reversible backup requirement: {message}"
                );
            }
            other => panic!("expected ExecFailed for missing DB, got {other:?}"),
        }
        assert!(
            !target.exists(),
            "DbExec must not create a fresh DB without a pre-mutation backup"
        );
        let actions = fs::read_to_string(ctx.run_dir.join("actions.jsonl")).unwrap();
        assert!(
            actions.is_empty(),
            "precondition refusal must not write pending/completed action records"
        );
    }

    #[test]
    fn g1_backup_path_for_handles_absolute_paths_outside_repo() {
        // `backup_path_for` must not use PathBuf::join with an absolute path
        // because that drops the base. Out-of-repo paths use __abs__/.
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
        // Op::Rename refuses if destination exists; silent overwrite of
        // existing files is forbidden.
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
        let actions = fs::read_to_string(
            td.path()
                .join(".doctor")
                .join("runs")
                .join("2026-05-09T16-30-15Z__g3")
                .join("actions.jsonl"),
        )
        .unwrap();
        let lines: Vec<serde_json::Value> = actions
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "refused rename must not leave only pending");
        assert_eq!(lines[1]["phase"], "completed");
        assert_eq!(lines[1]["ok"], false);
        assert!(
            lines[1]["error"]
                .as_str()
                .expect("error string")
                .contains("already exists")
        );
    }

    #[test]
    fn g4_atomic_write_chmod_via_fd_before_persist() {
        // Permissions are set via fd before persist, not via path after.
        // This checks the requested mode is applied through that path.
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
        // Dry-run must not hide would-be exit-4 refusals on out-of-scope
        // rename destinations.
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

    #[test]
    fn extra_locks_are_acquired_and_released() {
        let td = TempDir::new().unwrap();
        let extra_lock_path = td.path().join("project.lock");
        let mut ctx = make_ctx(&td, "2026-05-10T07-30-00Z__extra");
        ctx.extra_locks = vec![extra_lock_path.clone()];
        let target = td.path().join("hello.txt");
        let r = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"hi".to_vec(),
                mode: 0o644,
            },
        );
        assert!(r.is_ok(), "mutate should succeed when extra lock is free");
        // Lock file was created.
        assert!(extra_lock_path.exists());
        // After mutate returns, the extra lock guard dropped — verify
        // we can re-acquire it.
        let f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&extra_lock_path)
            .unwrap();
        assert!(
            f.try_lock_exclusive().is_ok(),
            "extra lock must be released after mutate returns"
        );
    }

    #[test]
    fn held_extra_lock_blocks_mutate() {
        let td = TempDir::new().unwrap();
        let extra_lock_path = td.path().join("project.lock");
        let mut ctx = make_ctx(&td, "2026-05-10T07-30-01Z__extrablock");
        ctx.extra_locks = vec![extra_lock_path.clone()];
        // Acquire the extra lock first as a "competing" process.
        fs::create_dir_all(td.path()).unwrap();
        let competitor = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&extra_lock_path)
            .unwrap();
        competitor.try_lock_exclusive().unwrap();
        let target = td.path().join("hello.txt");
        std::fs::write(&target, b"original").unwrap();
        // Now mutate should refuse.
        let err = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"new".to_vec(),
                mode: 0o644,
            },
        )
        .unwrap_err();
        assert!(matches!(err, MutateError::LockHeld(_)));
        // Target file untouched.
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        FileExt::unlock(&competitor).unwrap();
    }
}
