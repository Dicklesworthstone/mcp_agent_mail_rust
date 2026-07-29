//! Tamper-evident chain-of-custody manifest for `am doctor` runs (B3 /
//! `br-bvq1x.2.3`).
//!
//! ## Why this exists
//!
//! `am doctor undo <run-id>` replays `actions.jsonl` in reverse, restoring
//! each target from `backups/`. `undo.rs` already defends the *replay*:
//! it refuses `..`/prefix path components, re-applies the runtime
//! `write_scopes` trust boundary at restore time (`enforce_scope`), and
//! verifies per-action before/after hashes so it never clobbers a
//! user-modified file. Those defenses bound *where* undo can write and
//! whether the live target still looks post-mutation.
//!
//! What they do **not** prove is that the run artifacts themselves — the
//! `actions.jsonl` log and the `backups/` payload — are the exact bytes
//! the doctor produced. An attacker who can plant or edit
//! `.doctor/runs/<id>/` in a victim's repo (a malicious PR, a compromised
//! dependency, prior write access) can craft an internally-consistent run
//! that redirects undo to overwrite an in-scope file (an MCP config, the
//! mailbox config) with attacker-supplied bytes. Per-action hashes don't
//! help — the attacker controls both the log and the backups, so the
//! internal hashes agree.
//!
//! ## The mechanism
//!
//! At run-close the doctor seals a `manifest.json` that binds the run's
//! `actions.jsonl` and `backups/` under an **HMAC-SHA256** keyed by a
//! per-install secret kept *outside* any repo
//! (`$XDG_CONFIG_HOME/mcp-agent-mail/doctor-undo-hmac.key`, `0600`). A
//! repo-scoped attacker cannot read that key, so they cannot forge a
//! manifest for tampered artifacts. At undo time `verify_run_manifest`
//! recomputes the artifact hashes from disk, rebuilds the signed message,
//! and refuses (fail-closed) on any mismatch.
//!
//! Compatibility: runs sealed before this feature (or by flows that don't
//! produce undo-able runs) have no manifest. `ManifestVerdict::Absent`
//! preserves the prior behavior (warn + proceed) so undo of legacy /
//! in-flight runs is not broken. A *present* manifest must verify or undo
//! refuses — that is the B3 guarantee.
//!
//! ## Closing the manifest-omission downgrade (`br-q7f2b`)
//!
//! B3 left one gap: an attacker who can plant `.doctor/runs/<id>/` can
//! simply *omit* `manifest.json` so the verdict is `Absent` (warn +
//! proceed) instead of verify-or-refuse — a silent downgrade of a run the
//! doctor would otherwise have sealed. The defense is an **out-of-repo run
//! ledger** kept next to the HMAC key (config home, `0600`,
//! `doctor-undo-ledger.jsonl`). At seal time the doctor appends the run's
//! `(run_id, mac, sealed component hashes)`; at undo time, if the ledger
//! *knows* the run but the on-disk manifest is absent — or its key-less
//! re-check against the ledger's recorded hashes fails — undo refuses
//! (`ManifestVerdict::LedgerMismatch`, fail-closed). A repo-scoped attacker
//! cannot write the ledger, so they cannot make the doctor "forget" a run.
//! Runs the ledger has never seen (legacy, or sealed on another machine)
//! keep their on-disk verdict, so cross-machine recovery still flows
//! through `AM_DOCTOR_UNDO_ALLOW_UNVERIFIED`. As a bonus the ledger lets a
//! same-machine recovery whose key file was lost verify *without* the key
//! by binding the run's disk artifacts to the ledger's recorded hashes.
//!
//! HMAC-SHA256 is implemented inline over the already-present `sha2`
//! dependency — no new crypto crate, no `unsafe`.

#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File name of the sealed manifest inside a run directory.
pub const MANIFEST_FILE: &str = "manifest.json";
/// Current manifest schema version. Bump on any breaking format change.
pub const MANIFEST_VERSION: u32 = 1;
/// Length of a freshly-generated per-install key, in bytes.
const KEY_LEN: usize = 32;
/// Minimum acceptable key length when loading an existing key file; a
/// shorter file is treated as corrupt and regenerated at seal time.
const KEY_MIN_LEN: usize = 16;
/// Domain-separation prefix for the HMAC message so a manifest MAC can
/// never be confused with any other HMAC the project might compute.
const DOMAIN: &[u8] = b"mcp-agent-mail/doctor-undo-manifest/v1";
/// Env override for the key file location (tooling / hermetic tests).
pub const KEY_PATH_ENV: &str = "AM_DOCTOR_UNDO_KEY_FILE";
/// Env escape hatch: when set to a truthy value, undo proceeds despite an
/// unverifiable (not provably-tampered, e.g. key lost / cross-machine)
/// manifest. Documented recovery path; logs loudly.
pub const ALLOW_UNVERIFIED_ENV: &str = "AM_DOCTOR_UNDO_ALLOW_UNVERIFIED";

/// On-disk manifest. All hashes are `sha256:<hex>`; `hmac_sha256` is bare
/// hex (it is a MAC, not a content digest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub manifest_version: u32,
    pub run_id: String,
    /// SHA-256 of the exact bytes of `actions.jsonl`.
    pub actions_sha256: String,
    /// Order-independent digest over every file under `backups/`.
    pub backups_root_sha256: String,
    /// HMAC-SHA256(key, domain || run_id || actions_sha256 || backups_root_sha256).
    pub hmac_sha256: String,
}

/// Result of verifying a run's manifest at undo time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestVerdict {
    /// No manifest present — legacy / unsealed run. Caller proceeds with a
    /// warning (backward compatibility).
    Absent,
    /// Manifest present and HMAC verified against the per-install key.
    Verified,
    /// Manifest present but the per-install key is unavailable, so the
    /// chain of custody cannot be checked. Fail-closed unless overridden.
    KeyUnavailable(String),
    /// Manifest present but unreadable / unparseable / wrong version.
    /// Fail-closed unless overridden.
    Malformed(String),
    /// Manifest present and verification definitively failed: artifact
    /// hash drift or HMAC mismatch. Fail-closed unless overridden.
    Tampered(String),
    /// The out-of-repo run ledger recorded a sealed manifest for this run,
    /// but the on-disk manifest is absent or does not match what was sealed
    /// (the manifest-omission downgrade `br-q7f2b` closes). Fail-closed
    /// unless overridden. Distinct from `Tampered` so robot surfaces can
    /// tell "in-repo artifacts altered" from "the trusted ledger and the
    /// repo disagree about whether a manifest should exist".
    LedgerMismatch(String),
}

impl ManifestVerdict {
    /// Stable, machine-readable status label for robot/JSON surfaces.
    pub fn status_label(&self) -> &'static str {
        match self {
            ManifestVerdict::Absent => "unverified_legacy",
            ManifestVerdict::Verified => "verified",
            ManifestVerdict::KeyUnavailable(_) => "key_unavailable",
            ManifestVerdict::Malformed(_) => "malformed",
            ManifestVerdict::Tampered(_) => "tampered",
            ManifestVerdict::LedgerMismatch(_) => "ledger_mismatch",
        }
    }

    /// True when undo may proceed for this verdict, honoring the
    /// `AM_DOCTOR_UNDO_ALLOW_UNVERIFIED` escape hatch for the
    /// non-`Verified`/non-`Absent` cases.
    pub fn allows_replay(&self, allow_unverified: bool) -> bool {
        match self {
            ManifestVerdict::Verified | ManifestVerdict::Absent => true,
            ManifestVerdict::KeyUnavailable(_)
            | ManifestVerdict::Malformed(_)
            | ManifestVerdict::Tampered(_)
            | ManifestVerdict::LedgerMismatch(_) => allow_unverified,
        }
    }

    /// Human-readable detail for the non-passing verdicts (empty for the
    /// passing ones).
    pub fn detail(&self) -> &str {
        match self {
            ManifestVerdict::Absent | ManifestVerdict::Verified => "",
            ManifestVerdict::KeyUnavailable(d)
            | ManifestVerdict::Malformed(d)
            | ManifestVerdict::Tampered(d)
            | ManifestVerdict::LedgerMismatch(d) => d,
        }
    }
}

/// Whether the `AM_DOCTOR_UNDO_ALLOW_UNVERIFIED` escape hatch is engaged.
pub fn allow_unverified_from_env() -> bool {
    matches!(
        std::env::var(ALLOW_UNVERIFIED_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

// ----------------------------------------------------------------------
// HMAC-SHA256 (inline; no extra crypto dependency, no unsafe).
// ----------------------------------------------------------------------

const HMAC_BLOCK: usize = 64;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    // RFC 2104: keys longer than the block size are first hashed.
    let mut block = [0u8; HMAC_BLOCK];
    if key.len() > HMAC_BLOCK {
        let digest = Sha256::digest(key);
        block[..digest.len()].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; HMAC_BLOCK];
    let mut opad = [0x5cu8; HMAC_BLOCK];
    for (b, k) in ipad.iter_mut().zip(block.iter()) {
        *b ^= *k;
    }
    for (b, k) in opad.iter_mut().zip(block.iter()) {
        *b ^= *k;
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);

    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// Constant-time byte-slice equality — avoids a timing oracle on the MAC.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Append a length-prefixed field to `out` (u64-LE length, then bytes).
fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Canonical, unambiguous message that the HMAC authenticates. Each field
/// is length-prefixed so no concatenation collision is possible.
fn canonical_message(run_id: &str, actions_sha256: &str, backups_root_sha256: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN.len() + run_id.len() + 96);
    push_field(&mut out, DOMAIN);
    push_field(&mut out, run_id.as_bytes());
    push_field(&mut out, actions_sha256.as_bytes());
    push_field(&mut out, backups_root_sha256.as_bytes());
    out
}

// ----------------------------------------------------------------------
// Artifact hashing.
// ----------------------------------------------------------------------

/// Open a regular file for reading with `O_NOFOLLOW | O_NONBLOCK` on Unix
/// and a post-open `is_file()` check on the held fd. Mirrors `undo.rs`'s
/// hardened reads: defeats the symlink-swap and FIFO-swap TOCTOU on the
/// attacker-controllable run artifacts (`actions.jsonl`, `backups/`).
#[cfg(unix)]
fn open_regular_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(f)
}

#[cfg(not(unix))]
fn open_regular_no_follow(path: &Path) -> io::Result<fs::File> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to hash symlink {}", path.display()),
        ));
    }
    let f = OpenOptions::new().read(true).open(path)?;
    if !f.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(f)
}

/// Stream-hash a regular file in 64 KiB chunks (O(1) memory — multi-GB
/// SQLite backups hash without OOM) over a hardened, non-symlink fd.
fn hash_regular_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = open_regular_no_follow(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&hasher.finalize());
    Ok(arr)
}

/// `sha256:<hex>` digest of a regular file's bytes (hardened read).
fn sha256_file_stream(path: &Path) -> io::Result<String> {
    Ok(format!("sha256:{}", hex::encode(hash_regular_file(path)?)))
}

/// Order-independent digest over every entry under `backups/`.
///
/// Walks the tree without following symlinks. For each entry we record a
/// `(relative-path, kind, payload-hash)` triple, sort by relative path,
/// then fold the sorted, length-prefixed triples into a single SHA-256.
/// Files contribute a stream hash of their bytes; symlinks contribute a
/// hash of their (un-followed) target path, tagged distinctly — so a
/// swapped file *or* a planted symlink changes the root. A missing
/// `backups/` dir hashes to a stable empty-set sentinel.
fn backups_root_hash(backups_dir: &Path) -> io::Result<String> {
    let mut entries: Vec<(Vec<u8>, u8, [u8; 32])> = Vec::new();

    if backups_dir.exists() {
        for entry in walkdir::WalkDir::new(backups_dir)
            .follow_links(false)
            .into_iter()
        {
            let entry = entry.map_err(|e| io::Error::other(format!("walking backups: {e}")))?;
            let path = entry.path();
            if path == backups_dir {
                continue;
            }
            let ft = entry.file_type();
            if ft.is_dir() {
                continue;
            }
            let rel = path
                .strip_prefix(backups_dir)
                .map_err(|e| io::Error::other(format!("strip_prefix: {e}")))?;
            let rel_bytes = rel.to_string_lossy().into_owned().into_bytes();

            let (kind, payload): (u8, [u8; 32]) = if ft.is_symlink() {
                // Symlink: hash the (un-followed) target path, tagged
                // distinctly so a planted/swapped link changes the root.
                let target = fs::read_link(path)?;
                let mut h = Sha256::new();
                h.update(target.to_string_lossy().as_bytes());
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&h.finalize());
                (1u8, arr)
            } else if ft.is_file() {
                // Regular file: stream-hash bytes over a hardened fd.
                (0u8, hash_regular_file(path)?)
            } else {
                // FIFO / device / socket: a legit backups/ tree never
                // contains these. Record a path-only marker so the root
                // reflects the anomaly WITHOUT ever opening it (avoids a
                // FIFO-open DoS during verification).
                let mut h = Sha256::new();
                h.update(b"non-regular");
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&h.finalize());
                (2u8, arr)
            };
            entries.push((rel_bytes, kind, payload));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut root = Sha256::new();
    root.update(b"backups-root/v1");
    root.update((entries.len() as u64).to_le_bytes());
    for (rel, kind, payload) in &entries {
        root.update((rel.len() as u64).to_le_bytes());
        root.update(rel);
        root.update([*kind]);
        root.update(payload);
    }
    Ok(format!("sha256:{}", hex::encode(root.finalize())))
}

// ----------------------------------------------------------------------
// Seal / verify.
// ----------------------------------------------------------------------

/// Compute (but do not write) the manifest for a finalized run dir.
pub fn compute_manifest(run_dir: &Path, run_id: &str, key: &[u8]) -> io::Result<RunManifest> {
    let actions_sha256 = sha256_file_stream(&run_dir.join("actions.jsonl"))?;
    let backups_root_sha256 = backups_root_hash(&run_dir.join("backups"))?;
    let mac = hmac_sha256(
        key,
        &canonical_message(run_id, &actions_sha256, &backups_root_sha256),
    );
    Ok(RunManifest {
        manifest_version: MANIFEST_VERSION,
        run_id: run_id.to_string(),
        actions_sha256,
        backups_root_sha256,
        hmac_sha256: hex::encode(mac),
    })
}

/// Atomically write `manifest.json` (tmp + rename) into `run_dir`.
fn write_manifest_file(run_dir: &Path, manifest: &RunManifest) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    json.push(b'\n');
    atomic_write(&run_dir.join(MANIFEST_FILE), &json, 0o644)
}

/// Seal `manifest.json` into `run_dir` using an explicit key. The file is
/// written atomically (tmp + rename). Used by tests; production callers
/// use [`seal_run_manifest_default`].
pub fn seal_run_manifest_with_key(run_dir: &Path, run_id: &str, key: &[u8]) -> io::Result<()> {
    let manifest = compute_manifest(run_dir, run_id, key)?;
    write_manifest_file(run_dir, &manifest)
}

/// Seal `manifest.json` using the resolved per-install key, creating the
/// key on first use, and record the run in the out-of-repo ledger
/// (`br-q7f2b`) so a later manifest-omission downgrade is refused.
/// Best-effort: callers treat a failure as non-fatal (the run still
/// completes; undo falls back to the on-disk verdict for an unsealed run)
/// but should log it. The ledger write is itself best-effort within this
/// call — a missing ledger entry only weakens the downgrade defense for
/// this one run, it never breaks the seal.
pub fn seal_run_manifest_default(run_dir: &Path, run_id: &str) -> io::Result<()> {
    let key = load_or_create_undo_key()?;
    let manifest = compute_manifest(run_dir, run_id, &key)?;
    write_manifest_file(run_dir, &manifest)?;
    if let Err(e) = record_run_in_ledger_default(run_dir, &manifest) {
        eprintln!(
            "warning: sealed doctor manifest for {run_id} but could not record it in the \
             out-of-repo run ledger (downgrade defense weakened for this run): {e}"
        );
    }
    Ok(())
}

/// Verify a run's manifest with an explicit (optional) key.
pub fn verify_run_manifest(run_dir: &Path, run_id: &str, key: Option<&[u8]>) -> ManifestVerdict {
    let manifest_path = run_dir.join(MANIFEST_FILE);

    // Read without following a symlink — a symlinked manifest is not
    // something we ever wrote.
    match fs::symlink_metadata(&manifest_path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ManifestVerdict::Absent,
        Err(e) => return ManifestVerdict::Malformed(format!("stat manifest: {e}")),
        Ok(meta) if meta.file_type().is_symlink() => {
            return ManifestVerdict::Malformed("manifest.json is a symlink".to_string());
        }
        Ok(_) => {}
    }

    let bytes = match fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) => return ManifestVerdict::Malformed(format!("read manifest: {e}")),
    };
    let manifest: RunManifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => return ManifestVerdict::Malformed(format!("parse manifest: {e}")),
    };
    if manifest.manifest_version != MANIFEST_VERSION {
        return ManifestVerdict::Malformed(format!(
            "unsupported manifest_version {} (expected {MANIFEST_VERSION})",
            manifest.manifest_version
        ));
    }
    if manifest.run_id != run_id {
        return ManifestVerdict::Tampered(format!(
            "manifest run_id {:?} does not match run {:?}",
            manifest.run_id, run_id
        ));
    }

    let key = match key {
        Some(k) => k,
        None => {
            return ManifestVerdict::KeyUnavailable(
                "per-install doctor-undo key is unavailable; cannot verify chain of custody"
                    .to_string(),
            );
        }
    };

    // Recompute artifact hashes from disk and compare to the manifest's
    // claims (clear, specific drift messages), then verify the HMAC over
    // the recomputed values (authoritative — defeats a re-hashed but
    // un-re-signed manifest).
    let actions_sha256 = match sha256_file_stream(&run_dir.join("actions.jsonl")) {
        Ok(h) => h,
        Err(e) => return ManifestVerdict::Tampered(format!("actions.jsonl unreadable: {e}")),
    };
    if actions_sha256 != manifest.actions_sha256 {
        return ManifestVerdict::Tampered(format!(
            "actions.jsonl content drift (recomputed {actions_sha256} != sealed {})",
            manifest.actions_sha256
        ));
    }
    let backups_root_sha256 = match backups_root_hash(&run_dir.join("backups")) {
        Ok(h) => h,
        Err(e) => return ManifestVerdict::Tampered(format!("backups unreadable: {e}")),
    };
    if backups_root_sha256 != manifest.backups_root_sha256 {
        return ManifestVerdict::Tampered(
            "backups/ content drift versus sealed manifest".to_string(),
        );
    }

    let expected = hmac_sha256(
        key,
        &canonical_message(run_id, &actions_sha256, &backups_root_sha256),
    );
    let stored = match hex::decode(manifest.hmac_sha256.as_bytes()) {
        Ok(b) => b,
        Err(_) => return ManifestVerdict::Tampered("manifest hmac_sha256 is not hex".to_string()),
    };
    if !ct_eq(&expected, &stored) {
        return ManifestVerdict::Tampered(
            "manifest HMAC mismatch (artifacts altered, or sealed with a different key)"
                .to_string(),
        );
    }

    ManifestVerdict::Verified
}

/// Verify a run's manifest using the resolved per-install key.
pub fn verify_run_manifest_default(run_dir: &Path, run_id: &str) -> ManifestVerdict {
    let key = load_undo_key().unwrap_or(None);
    verify_run_manifest(run_dir, run_id, key.as_deref())
}

// ----------------------------------------------------------------------
// Per-install key management.
// ----------------------------------------------------------------------

/// The `mcp-agent-mail` directory under the user's config home. Honors
/// `XDG_CONFIG_HOME`, then `dirs::config_dir`, then `~/.config`. Always
/// outside any repo's `.doctor/` so a repo-scoped attacker cannot read or
/// write the per-install secrets stored here (the HMAC key, the run
/// ledger).
fn mcp_agent_mail_config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(dirs::config_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("mcp-agent-mail"))
}

/// Resolve the per-install key path. Honors `AM_DOCTOR_UNDO_KEY_FILE`,
/// then the XDG config home, then `~/.config`. Always outside any repo's
/// `.doctor/` so a repo-scoped attacker cannot read it.
pub fn undo_key_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(KEY_PATH_ENV)
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    Some(mcp_agent_mail_config_dir()?.join("doctor-undo-hmac.key"))
}

/// Load a key from an explicit path without creating it. Returns
/// `Ok(None)` when the file is absent or too short to be a real key.
pub fn load_undo_key_at(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("doctor-undo key {} is a symlink; refusing", path.display()),
        )),
        Ok(_) => {
            let bytes = fs::read(path)?;
            if bytes.len() < KEY_MIN_LEN {
                Ok(None)
            } else {
                Ok(Some(bytes))
            }
        }
    }
}

/// Load (or generate) a key at an explicit path. Generates a fresh
/// 32-byte key (0600) when absent or too short.
pub fn load_or_create_undo_key_at(path: &Path) -> io::Result<Vec<u8>> {
    if let Some(existing) = load_undo_key_at(path)? {
        return Ok(existing);
    }
    let mut key = vec![0u8; KEY_LEN];
    getrandom::fill(&mut key)
        .map_err(|e| io::Error::other(format!("getrandom for doctor-undo key: {e}")))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(path, &key, 0o600)?;
    Ok(key)
}

/// Load the per-install key without creating it. Returns `Ok(None)` when
/// the path can't be resolved or the file is absent. Used at undo time.
pub fn load_undo_key() -> io::Result<Option<Vec<u8>>> {
    let Some(path) = undo_key_path() else {
        return Ok(None);
    };
    load_undo_key_at(&path)
}

/// Load the per-install key, generating a fresh 32-byte key (0600) when
/// absent or too short. Used at seal time.
pub fn load_or_create_undo_key() -> io::Result<Vec<u8>> {
    let path =
        undo_key_path().ok_or_else(|| io::Error::other("cannot resolve doctor-undo key path"))?;
    load_or_create_undo_key_at(&path)
}

// ----------------------------------------------------------------------
// Out-of-repo run ledger (br-q7f2b).
//
// An append-only JSONL kept next to the HMAC key (config home, 0600). Each
// sealed run contributes one line binding `run_id` to the manifest's mac
// and the sealed component hashes. Because the ledger lives outside any
// repo, a repo-scoped attacker cannot edit it — so the doctor's own memory
// of "I sealed run X" survives even if the in-repo `manifest.json` is
// stripped. The ledger is consulted only as a *positive* signal (it can
// refuse a known run whose manifest vanished); a run the ledger has never
// seen is left to the on-disk verdict, preserving legacy + cross-machine
// behavior.
// ----------------------------------------------------------------------

/// File name (under config home) of the append-only run ledger.
pub const LEDGER_FILE: &str = "doctor-undo-ledger.jsonl";
/// Env override for the ledger file location (tooling / hermetic tests).
pub const LEDGER_PATH_ENV: &str = "AM_DOCTOR_UNDO_LEDGER_FILE";
/// Compact the ledger once it grows past this many bytes. Each entry is a
/// few hundred bytes, so ~4096 distinct runs fit under the cap; compaction
/// keeps the most recent [`LEDGER_KEEP_ENTRIES`] unique runs.
const LEDGER_MAX_BYTES: u64 = 1024 * 1024;
/// Unique runs to retain when compacting an oversized ledger (most-recent
/// wins). Comfortably above any realistic set of un-undone runs.
const LEDGER_KEEP_ENTRIES: usize = 2048;

/// One ledger record. `actions_sha256` / `backups_root_sha256` are the
/// `sha256:<hex>` component digests the manifest sealed; `hmac_sha256` is
/// the bare-hex MAC. Together they let undo re-check a key-lost run against
/// the trusted out-of-repo copy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerEntry {
    pub run_id: String,
    /// HMAC recorded at seal time (mirrors `RunManifest::hmac_sha256`).
    pub hmac_sha256: String,
    /// `sha256:<hex>` of the sealed `actions.jsonl`.
    pub actions_sha256: String,
    /// Order-independent `sha256:<hex>` of the sealed `backups/` tree.
    pub backups_root_sha256: String,
    /// Absolute repo path the run belonged to (best-effort; diagnostics).
    #[serde(default)]
    pub repo: String,
    /// Seal time in microseconds since the Unix epoch (best-effort; 0 when
    /// the clock is unavailable). Only used for human auditing — file
    /// append order is the authoritative recency signal.
    #[serde(default)]
    pub sealed_at_us: i64,
}

/// Resolve the run-ledger path. Honors `AM_DOCTOR_UNDO_LEDGER_FILE`, then
/// the config home (next to the HMAC key). Always outside any repo.
pub fn undo_ledger_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(LEDGER_PATH_ENV)
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    Some(mcp_agent_mail_config_dir()?.join(LEDGER_FILE))
}

/// Microseconds since the Unix epoch, or 0 when the clock is unavailable.
fn now_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .unwrap_or(0)
}

/// Best-effort repo root for a run dir (`<repo>/.doctor/runs/<id>`).
fn repo_root_of_run_dir(run_dir: &Path) -> String {
    run_dir
        .parent() // runs/
        .and_then(Path::parent) // .doctor/
        .and_then(Path::parent) // <repo>
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Sibling lock file that serializes ledger mutations (compaction + append)
/// across processes.
fn ledger_lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Append one record to the ledger at `path`, compacting first when the
/// file has grown past the cap.
///
/// The whole operation runs under an exclusive advisory lock on a sibling
/// `.lock` file. Holding the lock across both the (atomic-rename)
/// compaction *and* the append is what makes concurrent `am doctor` seals
/// correct: without it, a lock-free appender could hold an fd to the inode
/// a concurrent compaction orphans by rename, silently losing the append.
/// Seals are infrequent (once per `am doctor --fix`), so serializing them
/// costs nothing in practice. Readers ([`read_ledger_entries`]) take no
/// lock — compaction publishes via atomic rename and appends are
/// whole-line, so a reader always sees a consistent snapshot.
pub fn record_run_in_ledger_at(path: &Path, entry: &LedgerEntry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    use fs2::FileExt;
    // Held for the whole op; the advisory lock releases when `lock_file`
    // drops at function exit (after `f`, so the append lands before the
    // unlock). Locals drop in reverse declaration order.
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(ledger_lock_path(path))?;
    lock_file.lock_exclusive()?;

    compact_ledger_if_oversized(path)?;

    let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
    line.push(b'\n');

    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten to 0600 (it holds per-install run metadata).
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(&line)?;
    f.sync_data()?;
    drop(f);
    drop(lock_file);
    Ok(())
}

/// Record a sealed run in the per-install ledger. Best-effort: callers
/// treat a failure as non-fatal (undo simply falls back to the on-disk
/// verdict for this run) but should log it.
pub fn record_run_in_ledger_default(run_dir: &Path, manifest: &RunManifest) -> io::Result<()> {
    let path = undo_ledger_path()
        .ok_or_else(|| io::Error::other("cannot resolve doctor-undo ledger path"))?;
    let entry = LedgerEntry {
        run_id: manifest.run_id.clone(),
        hmac_sha256: manifest.hmac_sha256.clone(),
        actions_sha256: manifest.actions_sha256.clone(),
        backups_root_sha256: manifest.backups_root_sha256.clone(),
        repo: repo_root_of_run_dir(run_dir),
        sealed_at_us: now_us(),
    };
    record_run_in_ledger_at(&path, &entry)
}

/// Parse every well-formed line of the ledger at `path`. Malformed or
/// partially-written lines are skipped (forward-compat + crash tolerance).
/// Refuses to follow a symlinked ledger. Returns `Ok(vec![])` when absent.
fn read_ledger_entries(path: &Path) -> io::Result<Vec<LedgerEntry>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "doctor-undo ledger {} is a symlink; refusing",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
    }
    let bytes = fs::read(path)?;
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<LedgerEntry>(line) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Look up a run in the ledger at `path`. Last write wins (the ledger is
/// append-only, so the final matching line is the freshest). `Ok(None)`
/// when the ledger is absent or has no record of `run_id`.
pub fn lookup_run_in_ledger_at(path: &Path, run_id: &str) -> io::Result<Option<LedgerEntry>> {
    let entries = read_ledger_entries(path)?;
    Ok(entries.into_iter().rev().find(|e| e.run_id == run_id))
}

/// Look up a run in the per-install ledger. `Ok(None)` when the ledger
/// path can't be resolved, the file is absent, or the run is unknown.
pub fn lookup_run_in_ledger_default(run_id: &str) -> io::Result<Option<LedgerEntry>> {
    let Some(path) = undo_ledger_path() else {
        return Ok(None);
    };
    lookup_run_in_ledger_at(&path, run_id)
}

/// Compact the ledger when it exceeds [`LEDGER_MAX_BYTES`]: dedupe by
/// `run_id` keeping the most-recent occurrence, retain the newest
/// [`LEDGER_KEEP_ENTRIES`] unique runs, and atomically replace the file via
/// tmp + rename. The caller must hold the ledger lock (see
/// [`record_run_in_ledger_at`]). A no-op when the file is small or absent.
fn compact_ledger_if_oversized(path: &Path) -> io::Result<()> {
    let len = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if len <= LEDGER_MAX_BYTES {
        return Ok(());
    }

    let entries = read_ledger_entries(path)?;
    let kept = compact_entries(entries, LEDGER_KEEP_ENTRIES);

    let mut buf = Vec::new();
    for entry in &kept {
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        buf.extend_from_slice(&line);
    }
    atomic_write(path, &buf, 0o600)
}

/// Pure compaction core: dedupe by `run_id` keeping the most-recent
/// occurrence (append order is recency), retain at most `keep` unique runs
/// (newest), and return them in chronological (oldest-first) order ready to
/// rewrite. Extracted so the dedupe/cap/order policy is unit-testable
/// without building a multi-megabyte ledger.
fn compact_entries(entries: Vec<LedgerEntry>, keep: usize) -> Vec<LedgerEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<LedgerEntry> = Vec::new();
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.run_id.clone()) {
            kept.push(entry);
            if kept.len() >= keep {
                break;
            }
        }
    }
    kept.reverse();
    kept
}

// ----------------------------------------------------------------------
// Ledger-aware verdict resolution.
// ----------------------------------------------------------------------

/// First 16 hex chars of a mac, for compact diagnostics.
fn short_mac(mac: &str) -> &str {
    &mac[..mac.len().min(16)]
}

/// Re-verify a key-lost run against the trusted ledger record: recompute
/// the on-disk component hashes and compare them to the ledger's sealed
/// copies. The ledger lives outside the repo, so an equal comparison
/// proves the artifacts are exactly what the doctor sealed — no key
/// needed. Any drift (or unreadable artifact) is the ledger's "changed"
/// case → fail-closed.
fn ledger_keyless_verify(run_dir: &Path, run_id: &str, entry: &LedgerEntry) -> ManifestVerdict {
    let actions_sha256 = match sha256_file_stream(&run_dir.join("actions.jsonl")) {
        Ok(h) => h,
        Err(e) => {
            return ManifestVerdict::LedgerMismatch(format!(
                "run {run_id}: actions.jsonl unreadable for ledger re-check: {e}"
            ));
        }
    };
    if actions_sha256 != entry.actions_sha256 {
        return ManifestVerdict::LedgerMismatch(format!(
            "run {run_id}: actions.jsonl drifted from the out-of-repo ledger \
             (disk {actions_sha256} != ledger {})",
            entry.actions_sha256
        ));
    }
    let backups_root_sha256 = match backups_root_hash(&run_dir.join("backups")) {
        Ok(h) => h,
        Err(e) => {
            return ManifestVerdict::LedgerMismatch(format!(
                "run {run_id}: backups/ unreadable for ledger re-check: {e}"
            ));
        }
    };
    if backups_root_sha256 != entry.backups_root_sha256 {
        return ManifestVerdict::LedgerMismatch(format!(
            "run {run_id}: backups/ drifted from the out-of-repo ledger"
        ));
    }
    ManifestVerdict::Verified
}

/// Reconcile the on-disk manifest verdict with the out-of-repo ledger.
///
/// `ledger_entry` is the trusted record (or `None` when the ledger has
/// never seen this run). The rules:
///
/// * No ledger entry → the on-disk verdict stands (legacy run, or sealed on
///   another machine; cross-machine recovery still uses
///   `AM_DOCTOR_UNDO_ALLOW_UNVERIFIED`).
/// * Ledger entry + on-disk `Absent` → the manifest the doctor sealed has
///   been stripped: the exact downgrade this closes → `LedgerMismatch`.
/// * Ledger entry + on-disk `Verified` → cryptographically authentic
///   against the live key; trust it (only a key holder can forge that).
/// * Ledger entry + on-disk `KeyUnavailable` → key file lost but the ledger
///   survived: re-verify against the ledger's recorded hashes without the
///   key (`Verified` on match, `LedgerMismatch` on drift).
/// * Ledger entry + `Malformed`/`Tampered` → already fail-closed; the
///   ledger can't rescue a broken/forged manifest, so keep the verdict.
pub fn reconcile_with_ledger(
    run_dir: &Path,
    run_id: &str,
    disk_verdict: ManifestVerdict,
    ledger_entry: Option<&LedgerEntry>,
) -> ManifestVerdict {
    let Some(entry) = ledger_entry else {
        return disk_verdict;
    };
    match disk_verdict {
        ManifestVerdict::Absent => ManifestVerdict::LedgerMismatch(format!(
            "run {run_id}: the out-of-repo ledger recorded a sealed manifest \
             (mac {}), but no manifest.json is present — refusing a \
             manifest-omission downgrade",
            short_mac(&entry.hmac_sha256)
        )),
        ManifestVerdict::Verified => ManifestVerdict::Verified,
        ManifestVerdict::KeyUnavailable(_) => ledger_keyless_verify(run_dir, run_id, entry),
        other @ (ManifestVerdict::Malformed(_)
        | ManifestVerdict::Tampered(_)
        | ManifestVerdict::LedgerMismatch(_)) => other,
    }
}

/// Resolve the authoritative verdict for a run at undo time: the on-disk
/// manifest verdict reconciled against the per-install out-of-repo ledger.
/// This is what `am doctor undo` consults. A failure reading the ledger is
/// treated as "ledger unknown" (the on-disk verdict stands) so a config-home
/// hiccup never blocks an otherwise-verifiable undo.
pub fn resolve_manifest_verdict_default(run_dir: &Path, run_id: &str) -> ManifestVerdict {
    let disk = verify_run_manifest_default(run_dir, run_id);
    let ledger_entry = lookup_run_in_ledger_default(run_id).unwrap_or(None);
    reconcile_with_ledger(run_dir, run_id, disk, ledger_entry.as_ref())
}

// ----------------------------------------------------------------------
// Atomic write helper (tmp + fsync + rename), with explicit mode on Unix.
// ----------------------------------------------------------------------

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut f = tmp.as_file();
        f.write_all(bytes)?;
        f.sync_data()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|d| d.sync_all());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const KEY_A: &[u8] = b"key-a-0123456789-0123456789-abcd"; // 32 bytes
    const KEY_B: &[u8] = b"key-b-0123456789-0123456789-abcd"; // 32 bytes, different

    /// Build a minimal finalized run dir: actions.jsonl + backups/<file>.
    fn make_run(td: &TempDir) -> PathBuf {
        let run_dir = td.path().join("runs").join("2026-06-21T00-00-00Z__abc");
        fs::create_dir_all(run_dir.join("backups")).unwrap();
        fs::write(
            run_dir.join("actions.jsonl"),
            b"{\"path\":\"x\",\"op\":\"WriteFile\",\"before_hash\":\"\",\"ok\":true}\n",
        )
        .unwrap();
        fs::write(run_dir.join("backups").join("x"), b"original-bytes").unwrap();
        run_dir
    }

    #[test]
    fn hmac_matches_rfc2104_short_key_vector() {
        // RFC 4231 Test Case 2: key="Jefe", data="what do ya want ...".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn seal_then_verify_roundtrips_to_verified() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        assert!(run_dir.join(MANIFEST_FILE).is_file());
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert_eq!(verdict, ManifestVerdict::Verified, "got {verdict:?}");
    }

    #[test]
    fn absent_manifest_is_absent_verdict() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert_eq!(verdict, ManifestVerdict::Absent);
    }

    #[test]
    fn tampered_actions_log_is_detected() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        // Attacker rewrites the action log after sealing.
        fs::write(
            run_dir.join("actions.jsonl"),
            b"{\"path\":\"/etc/passwd\",\"op\":\"WriteFile\",\"before_hash\":\"\",\"ok\":true}\n",
        )
        .unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn tampered_backup_payload_is_detected() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        fs::write(run_dir.join("backups").join("x"), b"attacker-bytes").unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn planted_extra_backup_file_is_detected() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        fs::write(run_dir.join("backups").join("y"), b"new-file").unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn wrong_key_is_tampered_not_verified() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_B));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn forged_manifest_resigned_with_attacker_key_fails_against_install_key() {
        // Attacker rewrites actions.jsonl, recomputes component hashes, and
        // re-seals with THEIR key. Verification with the victim's install
        // key must still reject (the HMAC won't match).
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        fs::write(run_dir.join("actions.jsonl"), b"forged\n").unwrap();
        // Attacker re-seals consistently with their own key (KEY_B).
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_B).unwrap();
        // Victim verifies with the real install key (KEY_A) -> reject.
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn key_unavailable_when_no_key_supplied() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", None);
        assert!(
            matches!(verdict, ManifestVerdict::KeyUnavailable(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn run_id_mismatch_is_tampered() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        seal_run_manifest_with_key(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A).unwrap();
        let verdict = verify_run_manifest(&run_dir, "different-run-id", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Tampered(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn malformed_manifest_json_is_malformed_verdict() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        fs::write(run_dir.join(MANIFEST_FILE), b"{not json").unwrap();
        let verdict = verify_run_manifest(&run_dir, "2026-06-21T00-00-00Z__abc", Some(KEY_A));
        assert!(
            matches!(verdict, ManifestVerdict::Malformed(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn verdict_replay_policy_is_fail_closed_by_default() {
        assert!(ManifestVerdict::Verified.allows_replay(false));
        assert!(ManifestVerdict::Absent.allows_replay(false));
        assert!(!ManifestVerdict::Tampered("x".into()).allows_replay(false));
        assert!(!ManifestVerdict::Malformed("x".into()).allows_replay(false));
        assert!(!ManifestVerdict::KeyUnavailable("x".into()).allows_replay(false));
        // Escape hatch lets the non-Verified/non-Absent cases through.
        assert!(ManifestVerdict::Tampered("x".into()).allows_replay(true));
        assert!(ManifestVerdict::KeyUnavailable("x".into()).allows_replay(true));
    }

    #[test]
    fn load_or_create_key_is_stable_across_calls() {
        let td = TempDir::new().unwrap();
        let key_path = td.path().join("nested").join("undo.key");
        let k1 = load_or_create_undo_key_at(&key_path).unwrap();
        let k2 = load_or_create_undo_key_at(&key_path).unwrap();
        assert_eq!(k1.len(), KEY_LEN);
        assert_eq!(k1, k2, "key must persist across calls");
        // 0600 on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "key file must be 0600");
        }
    }

    #[test]
    fn freshly_generated_keys_differ() {
        let td = TempDir::new().unwrap();
        let k1 = load_or_create_undo_key_at(&td.path().join("k1")).unwrap();
        let k2 = load_or_create_undo_key_at(&td.path().join("k2")).unwrap();
        assert_ne!(k1, k2, "independent keys must be random/distinct");
    }

    #[test]
    fn empty_backups_dir_hashes_stably() {
        let td = TempDir::new().unwrap();
        let a = td.path().join("a").join("backups");
        let b = td.path().join("b").join("backups");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_eq!(
            backups_root_hash(&a).unwrap(),
            backups_root_hash(&b).unwrap()
        );
    }

    // ------------------------------------------------------------------
    // Out-of-repo run ledger (br-q7f2b). These tests drive the ledger via
    // explicit paths / values so they never touch the real config home and
    // never mutate process env — they are race-free under `cargo test`.
    // ------------------------------------------------------------------

    /// Build a `LedgerEntry` mirroring the manifest a sealed `run_dir`
    /// produced under `key`.
    fn ledger_entry_for(run_dir: &Path, run_id: &str, key: &[u8]) -> LedgerEntry {
        let m = compute_manifest(run_dir, run_id, key).unwrap();
        LedgerEntry {
            run_id: m.run_id,
            hmac_sha256: m.hmac_sha256,
            actions_sha256: m.actions_sha256,
            backups_root_sha256: m.backups_root_sha256,
            repo: "/repo".to_string(),
            sealed_at_us: 1,
        }
    }

    #[test]
    fn ledger_record_then_lookup_roundtrips() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let ledger = td.path().join("ledger.jsonl");
        let entry = ledger_entry_for(&run_dir, "2026-06-21T00-00-00Z__abc", KEY_A);
        record_run_in_ledger_at(&ledger, &entry).unwrap();
        let got = lookup_run_in_ledger_at(&ledger, "2026-06-21T00-00-00Z__abc").unwrap();
        assert_eq!(got.as_ref(), Some(&entry));
        // Unknown run is absent.
        assert_eq!(lookup_run_in_ledger_at(&ledger, "nope").unwrap(), None);
        // 0600 on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&ledger).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "ledger must be 0600");
        }
    }

    #[test]
    fn ledger_lookup_is_last_write_wins() {
        let td = TempDir::new().unwrap();
        let ledger = td.path().join("ledger.jsonl");
        let mk = |mac: &str| LedgerEntry {
            run_id: "dup".to_string(),
            hmac_sha256: mac.to_string(),
            actions_sha256: "sha256:a".to_string(),
            backups_root_sha256: "sha256:b".to_string(),
            repo: String::new(),
            sealed_at_us: 0,
        };
        record_run_in_ledger_at(&ledger, &mk("first")).unwrap();
        record_run_in_ledger_at(&ledger, &mk("second")).unwrap();
        let got = lookup_run_in_ledger_at(&ledger, "dup").unwrap().unwrap();
        assert_eq!(got.hmac_sha256, "second", "freshest record wins");
    }

    #[test]
    fn ledger_read_skips_malformed_lines() {
        let td = TempDir::new().unwrap();
        let ledger = td.path().join("ledger.jsonl");
        let good = LedgerEntry {
            run_id: "ok".to_string(),
            hmac_sha256: "m".to_string(),
            actions_sha256: "sha256:a".to_string(),
            backups_root_sha256: "sha256:b".to_string(),
            repo: String::new(),
            sealed_at_us: 0,
        };
        let good_json = serde_json::to_string(&good).unwrap();
        // A torn/garbage line between two valid lines must not break parsing.
        fs::write(&ledger, format!("{good_json}\n{{not json\n{good_json}\n")).unwrap();
        assert_eq!(lookup_run_in_ledger_at(&ledger, "ok").unwrap(), Some(good));
    }

    #[test]
    fn ledger_refuses_symlinked_file() {
        #[cfg(unix)]
        {
            let td = TempDir::new().unwrap();
            let real = td.path().join("real.jsonl");
            fs::write(&real, b"{}\n").unwrap();
            let link = td.path().join("ledger.jsonl");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(lookup_run_in_ledger_at(&link, "x").is_err());
        }
    }

    #[test]
    fn reconcile_no_ledger_entry_keeps_disk_verdict() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        for disk in [
            ManifestVerdict::Absent,
            ManifestVerdict::Verified,
            ManifestVerdict::KeyUnavailable("k".into()),
            ManifestVerdict::Tampered("t".into()),
        ] {
            let got = reconcile_with_ledger(&run_dir, "rid", disk.clone(), None);
            assert_eq!(got, disk, "no ledger entry must not change the verdict");
        }
    }

    #[test]
    fn reconcile_absent_with_ledger_entry_is_ledger_mismatch() {
        // The core downgrade defense: the ledger remembers a sealed run, but
        // the on-disk manifest has been stripped -> refuse.
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let run_id = "2026-06-21T00-00-00Z__abc";
        let entry = ledger_entry_for(&run_dir, run_id, KEY_A);
        let got = reconcile_with_ledger(&run_dir, run_id, ManifestVerdict::Absent, Some(&entry));
        assert!(
            matches!(got, ManifestVerdict::LedgerMismatch(_)),
            "got {got:?}"
        );
        assert!(!got.allows_replay(false), "must be fail-closed");
        assert!(got.allows_replay(true), "override must let it through");
    }

    #[test]
    fn reconcile_verified_with_ledger_entry_stays_verified() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let run_id = "2026-06-21T00-00-00Z__abc";
        let entry = ledger_entry_for(&run_dir, run_id, KEY_A);
        let got = reconcile_with_ledger(&run_dir, run_id, ManifestVerdict::Verified, Some(&entry));
        assert_eq!(got, ManifestVerdict::Verified);
    }

    #[test]
    fn reconcile_key_unavailable_recovers_via_ledger_when_artifacts_match() {
        // Key file lost but the out-of-repo ledger survived: undo verifies
        // without the key by binding disk artifacts to the ledger hashes.
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let run_id = "2026-06-21T00-00-00Z__abc";
        seal_run_manifest_with_key(&run_dir, run_id, KEY_A).unwrap();
        let entry = ledger_entry_for(&run_dir, run_id, KEY_A);
        // Disk verdict with no key is KeyUnavailable...
        assert!(matches!(
            verify_run_manifest(&run_dir, run_id, None),
            ManifestVerdict::KeyUnavailable(_)
        ));
        // ...but the ledger upgrades it to Verified (artifacts untouched).
        let got = reconcile_with_ledger(
            &run_dir,
            run_id,
            ManifestVerdict::KeyUnavailable("no key".into()),
            Some(&entry),
        );
        assert_eq!(got, ManifestVerdict::Verified, "got {got:?}");
    }

    #[test]
    fn reconcile_key_unavailable_refuses_when_artifacts_drift() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let run_id = "2026-06-21T00-00-00Z__abc";
        let entry = ledger_entry_for(&run_dir, run_id, KEY_A);
        // Attacker rewrites a backup after the ledger recorded the run.
        fs::write(run_dir.join("backups").join("x"), b"attacker-bytes").unwrap();
        let got = reconcile_with_ledger(
            &run_dir,
            run_id,
            ManifestVerdict::KeyUnavailable("no key".into()),
            Some(&entry),
        );
        assert!(
            matches!(got, ManifestVerdict::LedgerMismatch(_)),
            "got {got:?}"
        );
        assert!(!got.allows_replay(false));
    }

    #[test]
    fn reconcile_tampered_stays_tampered_even_with_ledger() {
        let td = TempDir::new().unwrap();
        let run_dir = make_run(&td);
        let run_id = "2026-06-21T00-00-00Z__abc";
        let entry = ledger_entry_for(&run_dir, run_id, KEY_A);
        let got = reconcile_with_ledger(
            &run_dir,
            run_id,
            ManifestVerdict::Tampered("forged".into()),
            Some(&entry),
        );
        assert!(matches!(got, ManifestVerdict::Tampered(_)), "got {got:?}");
    }

    #[test]
    fn ledger_mismatch_replay_policy_is_fail_closed_by_default() {
        assert!(!ManifestVerdict::LedgerMismatch("x".into()).allows_replay(false));
        assert!(ManifestVerdict::LedgerMismatch("x".into()).allows_replay(true));
        assert_eq!(
            ManifestVerdict::LedgerMismatch("x".into()).status_label(),
            "ledger_mismatch"
        );
        assert_eq!(
            ManifestVerdict::LedgerMismatch("why".into()).detail(),
            "why"
        );
    }

    #[test]
    fn compact_entries_dedupes_keeps_recent_and_orders() {
        let mk = |run: &str, mac: &str| LedgerEntry {
            run_id: run.to_string(),
            hmac_sha256: mac.to_string(),
            actions_sha256: "sha256:a".to_string(),
            backups_root_sha256: "sha256:b".to_string(),
            repo: String::new(),
            sealed_at_us: 0,
        };
        // Append order (oldest -> newest): A1, B1, A2, C1, B2.
        let entries = vec![
            mk("A", "A1"),
            mk("B", "B1"),
            mk("A", "A2"),
            mk("C", "C1"),
            mk("B", "B2"),
        ];
        // keep=2 unique runs, most-recent: the freshest two distinct run_ids
        // are C (C1) and B (B2); A drops. Output is chronological.
        let kept = compact_entries(entries, 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].run_id, "C");
        assert_eq!(kept[1].run_id, "B");
        assert_eq!(kept[1].hmac_sha256, "B2", "dedupe keeps the latest mac");
    }

    #[test]
    fn ledger_compacts_when_oversized() {
        let td = TempDir::new().unwrap();
        let ledger = td.path().join("ledger.jsonl");
        // Pad each line so a few hundred entries blow past LEDGER_MAX_BYTES,
        // using a SMALL set of run_ids so compaction must dedupe heavily.
        let pad = "p".repeat(4096);
        for i in 0..600u32 {
            let entry = LedgerEntry {
                run_id: format!("run-{}", i % 8),
                hmac_sha256: format!("mac-{i}"),
                actions_sha256: "sha256:a".to_string(),
                backups_root_sha256: "sha256:b".to_string(),
                repo: pad.clone(),
                sealed_at_us: i64::from(i),
            };
            record_run_in_ledger_at(&ledger, &entry).unwrap();
        }
        let entries = read_ledger_entries(&ledger).unwrap();
        // Only 8 distinct run_ids ever existed; compaction dedupes, so the
        // live file never carries more than that many unique runs.
        let distinct: std::collections::HashSet<_> =
            entries.iter().map(|e| e.run_id.as_str()).collect();
        assert!(
            distinct.len() <= 8,
            "expected <=8 unique runs after compaction, got {}",
            distinct.len()
        );
        // 600 padded lines uncompacted would be ~2.5 MiB; staying well under
        // that proves at least one compaction pass ran.
        let size = fs::metadata(&ledger).unwrap().len();
        assert!(
            size < LEDGER_MAX_BYTES * 3 / 2,
            "ledger should have been compacted; size = {size}"
        );
        // The freshest record for a run_id is still resolvable and current.
        let got = lookup_run_in_ledger_at(&ledger, "run-0").unwrap().unwrap();
        assert_eq!(got.hmac_sha256, "mac-592", "freshest run-0 is i=592");
    }
}
