//! `fm-secrets-env-state-committed-env-file-in-repo` — P0.
//!
//! **Subsystem**: secrets_env_state.
//!
//! ## What's broken
//!
//! `.env`-style files containing bearer tokens (or other secrets) get
//! committed to the project's git repository. Two distinct hazards:
//!
//! 1. **Tracked-by-git**: the file is in `git ls-files`. The token is
//!    in the repo history; chmod is useless — the leak is already in
//!    every clone. Remediation requires git history hygiene which is
//!    well outside the doctor's scope (see [RULE 1: NO FILE DELETION]
//!    and "Irreversible Git & Filesystem Actions" in `AGENTS.md`).
//!
//! 2. **Untracked-but-present-with-token-shape**: the project's
//!    `.env` / `.env.local` / `.env.production` etc. exists in the
//!    project root with token-shape content, but is gitignored. Still
//!    a hygiene risk if the file has a wider-than-0o600 mode (any
//!    local user can read it). This branch IS auto-fixable.
//!
//! ## Detector
//!
//! Two lanes, returning one finding per problematic file:
//!
//! - **Tracked lane**: `git ls-files -z` from `project_root`; filter
//!   to basenames matching `.env*` or `*.env` or `config.env`; for
//!   each, read first 4 KiB and check for token-shape content; emit
//!   a finding tagged `tracked_by_git: true`.
//!
//! - **Untracked lane**: probe a small list of well-known `.env*`
//!   names in `project_root`; for any that exist, are NOT tracked,
//!   AND contain token-shape content, emit a finding tagged
//!   `tracked_by_git: false`.
//!
//! Detector is PURE — only reads disk + invokes `git ls-files` (a
//! read-only git query).
//!
//! ## Fix (`Op::Chmod`)
//!
//! - **Tracked**: refuse to mutate. The fixer returns
//!   `actions_skipped: 1` and the registry's `manual_remediation`
//!   field points operators at the
//!   `git rm --cached <file> && am setup --rotate-token` workflow.
//!
//! - **Untracked + token-shape**: chmod to `0o600`. The mode gate
//!   lives in **two places** for defense-in-depth (pass-35T review
//!   F3 / F4):
//!
//!   1. **Detector** (`mode & 0o077 == 0` precondition): owner-only
//!      modes never produce a finding in the first place, so
//!      already-strict files never enter the fix path.
//!   2. **Fixer** (live re-stat before mutate): protects against a
//!      TOCTOU window where the file is tightened between
//!      `detect()` and `fix()`. Without this, a 0o644 finding plus
//!      an external `chmod 0o400` between phases would still
//!      result in the fixer broadening the file to 0o600.
//!
//!   The Op::Rename-to-quarantine variant documented in the
//!   repair_spec is intentionally NOT implemented in this first
//!   cut — chmod is the minimum-action lane and matches the
//!   existing `world_readable_token_bak` pattern.
//!
//! ## Privacy
//!
//! The detector reads the first 4 KiB of each candidate file to scan
//! for token-shape patterns, but only the matched pattern label
//! (e.g., `"HTTP_BEARER_TOKEN="`) is stored on the finding — never
//! the token bytes. The manual_remediation envelope likewise refers
//! to the file by path only.
//!
//! ## Reversibility
//!
//! `am doctor undo <run-id>` reads `actions.jsonl`'s `before_mode`
//! field and restores the original mode. Tracked-lane findings have
//! no mutation to undo.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use mcp_agent_mail_core::git_cmd::GitCmd;
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-secrets-env-state-committed-env-file-in-repo";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "secrets_env_state";

/// Target mode for the untracked-lane chmod.
pub const SAFE_MODE: u32 = 0o600;

/// Bytes of each candidate file scanned for token shape. 4 KiB
/// matches the repair_spec; large enough to catch a `.env` with a
/// token line near the bottom of a typical file, small enough that a
/// big binary `.env`-suffixed file (e.g., someone misnamed a
/// model-weights blob) doesn't blow up memory.
const SCAN_PREFIX_BYTES: usize = 4096;

/// Token-shape patterns. Matched case-insensitively against the
/// file's first 4 KiB. Mirrors the patterns used by
/// `world_readable_token_bak` for consistency across the
/// secrets_env_state subsystem.
const TOKEN_SHAPE_PATTERNS: &[&str] = &[
    "HTTP_BEARER_TOKEN",
    "authorization=",
    "bearer=",
    "token=",
    "secret=",
    "api_key=",
    "\"authorization\"",
    "\"bearer\"",
    "\"token\"",
];

/// Well-known untracked `.env`-style filenames probed in
/// `project_root`. Not exhaustive — the tracked lane handles the
/// long tail via `git ls-files`. This list catches the canonical
/// names that operators forget to add to `.gitignore`.
///
/// `.envrc` (direnv) is included even though it's a shell script
/// rather than a strict dotenv file — direnv `export FOO=bar`
/// lines hold secrets in the same operational role as `.env`,
/// and operators reasonably expect this FM to catch them.
const UNTRACKED_PROBE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.test",
    ".env.staging",
    ".envrc",
    "config.env",
];

#[derive(Debug, Clone, Serialize)]
pub struct CommittedEnvFileFinding {
    pub path: PathBuf,
    /// `true` for files returned by `git ls-files`. The fixer NEVER
    /// mutates these.
    pub tracked_by_git: bool,
    /// Current file mode (low 9 bits), as read by `symlink_metadata`.
    pub current_mode: u32,
    /// First token-shape pattern matched in the file's first 4 KiB.
    /// Pattern *label*, never the token bytes.
    pub matched_pattern: String,
}

impl CommittedEnvFileFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = if self.tracked_by_git {
            format!(
                "env file {} is tracked by git AND contains token-shape content (history leak; manual `git rm --cached` required)",
                self.path.display(),
            )
        } else {
            format!(
                "untracked env file {} contains token-shape content with mode 0o{:o} (target: 0o{:o})",
                self.path.display(),
                self.current_mode & 0o777,
                SAFE_MODE,
            )
        };
        let auto_fixable = !self.tracked_by_git;
        let estimated_actions = if auto_fixable { 1 } else { 0 };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "path": self.path.to_string_lossy(),
                "tracked_by_git": self.tracked_by_git,
                "current_mode_octal": format!("0o{:o}", self.current_mode & 0o777),
                "target_mode_octal": format!("0o{:o}", SAFE_MODE),
                "matched_pattern": self.matched_pattern,
                "manual_remediation": if self.tracked_by_git {
                    serde_json::json!({
                        "steps": [
                            format!("git rm --cached {}", self.path.display()),
                            "echo '<file>' >> .gitignore",
                            "git commit -m 'remove tracked env file from index'",
                            "am setup --rotate-token  # rotate any leaked credentials",
                        ],
                        "warning": "The token is still present in git history. Consider history rewriting (e.g., git filter-repo) and a full credential rotation.",
                    })
                } else {
                    serde_json::json!(null)
                },
            }),
            remediation: FindingRemediation {
                command: if auto_fixable {
                    format!("am doctor --fix --only {FM_ID} --yes")
                } else {
                    "manual remediation required — see evidence.manual_remediation".to_string()
                },
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable,
                estimated_actions,
            },
        }
    }
}

/// Returns true if `basename` matches a `.env`-style filename
/// convention worth scanning. Exposed (`pub(crate)`) so tests can
/// pin the matcher behavior independently of the detector.
///
/// Matches: `.env`, `.env.<suffix>`, `config.env`, `<prefix>.env`,
/// and `.envrc` (direnv shell config; technically not dotenv
/// format but operationally a common secrets-bearing file —
/// see pass-35S review F2).
pub(crate) fn is_env_basename(basename: &str) -> bool {
    if basename == ".env" || basename == "config.env" || basename == ".envrc" {
        return true;
    }
    if basename.starts_with(".env.") {
        return true;
    }
    // `prod.env`, `staging.env`, etc. — `.env` suffix without a
    // leading dot is the canonical "dotenv-format" naming
    // convention in many tools.
    basename.ends_with(".env") && !basename.starts_with('.')
}

/// Reads up to `SCAN_PREFIX_BYTES` from `path` and returns the
/// matched token-shape pattern (if any). Case-insensitive match.
fn token_pattern_in_head(path: &Path) -> Option<String> {
    let mut buf = Vec::with_capacity(SCAN_PREFIX_BYTES);
    use std::io::Read;
    let f = fs::File::open(path).ok()?;
    let mut take = f.take(SCAN_PREFIX_BYTES as u64);
    take.read_to_end(&mut buf).ok()?;
    let lowered = String::from_utf8_lossy(&buf).to_lowercase();
    for pat in TOKEN_SHAPE_PATTERNS {
        if lowered.contains(&pat.to_lowercase()) {
            return Some((*pat).to_string());
        }
    }
    None
}

/// Runs `git ls-files -z` from `project_root` and returns the set
/// of relative paths git considers tracked. Returns `None` if the
/// git binary fails to invoke, or if `ls-files` exits non-zero
/// (e.g., not a git repo). Caller treats `None` as "tracked lane
/// unavailable; skip it" — the untracked lane still runs.
///
/// Note: we deliberately do NOT pre-check `project_root.join(".git").exists()`.
/// `git ls-files` works from any subdirectory of a git checkout
/// (it walks up to find `.git`). Pre-gating on a literal `.git`
/// path at `project_root` would mis-classify tracked files in
/// repo subdirectories as untracked — see pass-35S review
/// finding F1 (Gemini).
fn list_tracked_files(project_root: &Path) -> Option<Vec<PathBuf>> {
    let out = GitCmd::new(project_root)
        .args(["ls-files", "-z"])
        .run()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let entries = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(std::ffi::OsStr::from_bytes(s)))
        .collect();
    Some(entries)
}

// Linux/macOS: bytes ↔ OsStr conversion is from std::os::unix.
use std::os::unix::ffi::OsStrExt;

/// Detector. PURE.
///
/// Enumerates tracked `.env`-style files via `git ls-files -z`, then
/// probes a small list of well-known untracked `.env*` basenames in
/// `project_root`. Emits one finding per problematic file.
pub fn detect(project_root: &Path) -> Vec<CommittedEnvFileFinding> {
    let mut out: Vec<CommittedEnvFileFinding> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // Tracked lane: walk git ls-files output.
    let tracked = list_tracked_files(project_root);
    if let Some(entries) = &tracked {
        for rel in entries {
            let basename = match rel.file_name().and_then(|s| s.to_str()) {
                Some(b) => b,
                None => continue,
            };
            if !is_env_basename(basename) {
                continue;
            }
            let abs = project_root.join(rel);
            // Reject symlinks defensively — git ls-files lists what's
            // in the index; a malicious symlink on disk pointing at
            // /etc/shadow should not become evidence.
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let Some(pattern) = token_pattern_in_head(&abs) else {
                continue;
            };
            seen.insert(abs.clone());
            out.push(CommittedEnvFileFinding {
                path: abs,
                tracked_by_git: true,
                current_mode: meta.permissions().mode(),
                matched_pattern: pattern,
            });
        }
    }

    // Untracked lane: probe well-known names in project_root.
    //
    // **Mode gate** (pass-35S review F1, Codex): only emit a finding
    // when the file has group/other readable/writable bits set
    // (i.e., `mode & 0o077 != 0`). Without this gate, a token-shape
    // file already at `0o400` or `0o500` would be flagged and the
    // fixer would chmod it to `0o600` — *broadening* permissions.
    // Tracked lane is intentionally NOT gated on mode; the leak is
    // in git history regardless of disk permissions.
    for name in UNTRACKED_PROBE_NAMES {
        let candidate = project_root.join(name);
        if seen.contains(&candidate) {
            continue; // already flagged via tracked lane
        }
        let meta = match fs::symlink_metadata(&candidate) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            // Already owner-only or stricter; chmod would not improve
            // the situation. Skip silently — re-running detect after
            // an external chmod would already be a no-op.
            continue;
        }
        let Some(pattern) = token_pattern_in_head(&candidate) else {
            continue;
        };
        out.push(CommittedEnvFileFinding {
            path: candidate,
            tracked_by_git: false,
            current_mode: mode,
            matched_pattern: pattern,
        });
    }

    out
}

/// Fixer. Routes untracked-lane findings through `mutate()` with
/// `Op::Chmod`. Tracked-lane findings are no-ops (manual
/// remediation only).
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &CommittedEnvFileFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if finding.tracked_by_git {
        // Detect-only lane — never mutate tracked files.
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    // Live re-stat at fix time. `finding.current_mode` is the
    // detect-phase snapshot; the file may have been tightened or
    // disappeared between detect() and fix(). Without this gate,
    // a stale 0o644 finding plus an external `chmod 0o400` would
    // still cause the fixer to broaden the file to 0o600 — the
    // exact permission-widening bug the pass-35S detector gate
    // tried to prevent (pass-35T review F3, both models).
    let live_mode = match fs::symlink_metadata(&finding.path) {
        Ok(m) if m.file_type().is_file() => m.permissions().mode(),
        Ok(_) => {
            // Symlink or other non-regular file at the target. Refuse
            // to follow — same defense as the detector.
            return Ok(FixOutcome {
                actions_taken: 0,
                actions_skipped: 1,
                quarantined_paths: Vec::new(),
            });
        }
        Err(_) => {
            // File vanished between detect and fix.
            return Ok(FixOutcome {
                actions_taken: 0,
                actions_skipped: 1,
                quarantined_paths: Vec::new(),
            });
        }
    };
    if live_mode & 0o077 == 0 {
        // Owner-only or stricter already (including exact SAFE_MODE).
        // chmod(0o600) would only widen 0o400 / 0o500 modes here, so
        // skip. The detector should have caught this; the live
        // re-stat is the second backstop.
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    mutate(ctx, &finding.path, Op::Chmod { mode: SAFE_MODE })?;
    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use crate::doctor::runs::scaffold_run_dir;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
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
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    fn write_with_mode(path: &Path, content: &str, mode: u32) {
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn is_env_basename_matches_canonical_patterns() {
        assert!(is_env_basename(".env"));
        assert!(is_env_basename(".env.local"));
        assert!(is_env_basename(".env.production"));
        assert!(is_env_basename("config.env"));
        assert!(is_env_basename("prod.env"));
        assert!(is_env_basename("staging.env"));
    }

    #[test]
    fn is_env_basename_matches_envrc_for_direnv() {
        // pass-35S review F2 (Codex + Gemini): direnv config files
        // hold secrets in the same operational role as .env.
        assert!(is_env_basename(".envrc"));
    }

    #[test]
    fn is_env_basename_rejects_unrelated() {
        assert!(!is_env_basename("env.txt"));
        assert!(!is_env_basename("README.md"));
        assert!(!is_env_basename(".envfile")); // no separator after .env
        assert!(!is_env_basename("foo.environment"));
    }

    #[test]
    fn detect_returns_empty_for_non_git_dir() {
        let td = TempDir::new().unwrap();
        // No .git/ → tracked lane skipped; no .env* probes match.
        let findings = detect(td.path());
        assert!(findings.is_empty());
    }

    #[test]
    fn detect_finds_untracked_dotenv_with_token() {
        let td = TempDir::new().unwrap();
        write_with_mode(
            &td.path().join(".env"),
            "HTTP_BEARER_TOKEN=secret-token-12345\nFOO=bar\n",
            0o644,
        );
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].tracked_by_git);
        assert_eq!(findings[0].current_mode & 0o777, 0o644);
        assert!(findings[0].matched_pattern.contains("HTTP_BEARER_TOKEN"));
    }

    #[test]
    fn detect_skips_untracked_dotenv_without_token_shape() {
        let td = TempDir::new().unwrap();
        write_with_mode(
            &td.path().join(".env"),
            "FOO=bar\nNODE_ENV=production\n",
            0o644,
        );
        let findings = detect(td.path());
        assert!(findings.is_empty(), "no token-shape → no finding");
    }

    #[test]
    fn detect_finds_multiple_untracked_env_variants() {
        let td = TempDir::new().unwrap();
        write_with_mode(&td.path().join(".env"), "HTTP_BEARER_TOKEN=token-a", 0o644);
        write_with_mode(
            &td.path().join(".env.local"),
            "secret=token-b",
            0o660, // group-readable → still flagged
        );
        let findings = detect(td.path());
        assert_eq!(findings.len(), 2);
        for f in &findings {
            assert!(!f.tracked_by_git);
        }
    }

    #[test]
    fn detect_skips_untracked_env_already_at_owner_only_mode() {
        // pass-35S review F1 (Codex): the detector previously emitted
        // a finding for any token-shape file regardless of mode. The
        // fixer would then chmod a 0o400 / 0o500 file to 0o600,
        // BROADENING permissions. The mode gate `mode & 0o077 == 0`
        // means owner-only modes are now silently skipped.
        let td = TempDir::new().unwrap();
        write_with_mode(
            &td.path().join(".env"),
            "HTTP_BEARER_TOKEN=already-protected",
            0o400, // stricter than the safe mode; must not be flagged
        );
        let findings = detect(td.path());
        assert!(
            findings.is_empty(),
            "0o400 file must not be flagged — fixer would otherwise widen to 0o600"
        );
    }

    #[test]
    fn detect_skips_untracked_env_at_exactly_safe_mode() {
        let td = TempDir::new().unwrap();
        write_with_mode(
            &td.path().join(".env"),
            "HTTP_BEARER_TOKEN=secret",
            0o600, // already at SAFE_MODE
        );
        let findings = detect(td.path());
        assert!(findings.is_empty(), "0o600 file is already safe");
    }

    #[test]
    fn detect_finds_envrc_with_token() {
        // pass-35S review F2 (Codex + Gemini): direnv .envrc is a
        // common secrets-bearing file. It's in UNTRACKED_PROBE_NAMES
        // and must be flagged when world/group-readable.
        let td = TempDir::new().unwrap();
        write_with_mode(
            &td.path().join(".envrc"),
            "export HTTP_BEARER_TOKEN=secret-from-direnv\n",
            0o644,
        );
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert!(findings[0].path.ends_with(".envrc"));
        assert!(!findings[0].tracked_by_git);
    }

    #[test]
    fn detect_skips_symlink_candidates() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.env");
        write_with_mode(&real, "HTTP_BEARER_TOKEN=x", 0o644);
        let link = td.path().join(".env");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let findings = detect(td.path());
        // .env is the symlink → rejected. real.env is not on the
        // probe list and project isn't a git repo, so tracked lane
        // doesn't see it either.
        assert!(findings.is_empty());
    }

    #[test]
    fn finding_serializes_with_tracked_flag_and_no_token_bytes() {
        let f = CommittedEnvFileFinding {
            path: "/repo/.env".into(),
            tracked_by_git: true,
            current_mode: 0o100644,
            matched_pattern: "HTTP_BEARER_TOKEN".into(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"tracked_by_git\":true"));
        assert!(s.contains("\"current_mode_octal\":\"0o644\""));
        assert!(s.contains("manual_remediation"));
        assert!(s.contains("git rm --cached"));
        // The matched pattern label appears, but no token bytes
        // exist on the finding to leak.
        assert!(s.contains("HTTP_BEARER_TOKEN"));
        // The pattern label is the only place HTTP_BEARER_TOKEN
        // appears; assert no value-after-equals snippet leaked.
        assert!(!s.contains("secret-token"));
    }

    #[test]
    fn finding_untracked_lane_is_auto_fixable() {
        let f = CommittedEnvFileFinding {
            path: "/repo/.env".into(),
            tracked_by_git: false,
            current_mode: 0o100644,
            matched_pattern: "secret=".into(),
        };
        let g = f.to_finding();
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
        assert!(g.remediation.command.contains("--fix"));
    }

    #[test]
    fn finding_tracked_lane_is_not_auto_fixable() {
        let f = CommittedEnvFileFinding {
            path: "/repo/.env".into(),
            tracked_by_git: true,
            current_mode: 0o100644,
            matched_pattern: "secret=".into(),
        };
        let g = f.to_finding();
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        assert!(g.remediation.command.contains("manual"));
    }

    #[test]
    fn fixer_chmods_untracked_finding_to_0o600() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o644);
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        let ctx = ctx_for(&td, "2026-05-15T09-30-00Z__fix-untracked");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);
        let new_mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(new_mode & 0o777, 0o600);
    }

    #[test]
    fn fixer_skips_tracked_finding_without_mutating() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o644);
        let finding = CommittedEnvFileFinding {
            path: p.clone(),
            tracked_by_git: true,
            current_mode: 0o100644,
            matched_pattern: "HTTP_BEARER_TOKEN".into(),
        };
        let ctx = ctx_for(&td, "2026-05-15T09-30-01Z__skip-tracked");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // Mode must be unchanged.
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o644,
            "tracked-lane fixer must not mutate the file"
        );
    }

    #[test]
    fn fixer_idempotent_on_already_safe_mode() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o600);
        let finding = CommittedEnvFileFinding {
            path: p.clone(),
            tracked_by_git: false,
            current_mode: 0o100600,
            matched_pattern: "HTTP_BEARER_TOKEN".into(),
        };
        let ctx = ctx_for(&td, "2026-05-15T09-30-02Z__idem");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn fixer_skips_missing_path() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("nonexistent.env");
        let finding = CommittedEnvFileFinding {
            path: p,
            tracked_by_git: false,
            current_mode: 0o100644,
            matched_pattern: "secret=".into(),
        };
        let ctx = ctx_for(&td, "2026-05-15T09-30-03Z__missing");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn fixer_then_undo_restores_original_mode() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o644);
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        let run_id = "2026-05-15T09-30-04Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(ctx);
        let summary = crate::doctor::undo::run_undo_with_scopes(
            td.path(),
            run_id,
            false,
            true,
            &[td.path().to_path_buf()],
        )
        .expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        let restored = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            restored & 0o777,
            0o644,
            "undo must restore original 0o644 mode"
        );
    }

    #[test]
    fn token_pattern_in_head_matches_multiple_shapes() {
        let td = TempDir::new().unwrap();
        for (body, expected_substr) in [
            ("HTTP_BEARER_TOKEN=secret", "HTTP_BEARER_TOKEN"),
            ("Authorization=Bearer x", "authorization="),
            ("bearer=abc", "bearer="),
            ("api_key=zyx", "api_key="),
            (r#"{"token":"v"}"#, "\"token\""),
            (r#"{"AUTHORIZATION":"x"}"#, "\"authorization\""),
        ] {
            let p = td.path().join("probe.env");
            fs::write(&p, body).unwrap();
            let pattern = token_pattern_in_head(&p).expect("must match");
            assert!(
                pattern.contains(expected_substr),
                "body {body:?} expected to contain {expected_substr:?}, got {pattern:?}"
            );
            fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn token_pattern_in_head_misses_pure_noise() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("probe.env");
        fs::write(&p, "NODE_ENV=production\nPORT=8080\nHOST=localhost\n").unwrap();
        assert!(token_pattern_in_head(&p).is_none());
    }

    /// Helper: initialize a temp git repo via `git init` so we can
    /// exercise the tracked lane through the real git binary
    /// (pass-35S review F3, Gemini). Test isolation env vars
    /// pulled out so every git invocation in this module shares
    /// the same hermetic setup (pass-35T review F2 — added
    /// `GIT_CONFIG_NOSYSTEM=1` and dropped the `-b main` flag
    /// which requires git 2.28+ and isn't needed since we never
    /// commit).
    fn hermetic_git(args: &[&str], dir: &Path) -> std::process::ExitStatus {
        // Test-only hardening (pass-35U review F1, Codex): clear
        // GIT_INDEX_FILE / GIT_OBJECT_DIRECTORY / GIT_TEMPLATE_DIR /
        // GIT_ALTERNATE_OBJECT_DIRECTORIES from the parent env to
        // prevent unusual developer or CI environments from
        // perturbing `git add` / `git ls-files`. XDG_CONFIG_HOME
        // also redirects config lookup paths in some git versions.
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_TEMPLATE_DIR")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("XDG_CONFIG_HOME")
            .status()
            .expect("git invocation")
    }

    fn init_git_repo(dir: &Path) {
        // `-q` keeps output quiet. We deliberately do NOT pass
        // `-b <branch>` (added in git 2.28; older systems would
        // fail with "unknown switch 'b'"). The default branch
        // name is irrelevant because the tests only run
        // `git add` and never commit.
        let status = hermetic_git(&["init", "-q"], dir);
        assert!(status.success(), "git init failed");
        // Minimal config so `git add` and any later `git commit`
        // work without name/email errors.
        for (k, v) in [
            ("user.name", "doctor-test"),
            ("user.email", "doctor@test.example"),
        ] {
            assert!(
                hermetic_git(&["config", k, v], dir).success(),
                "git config {k} failed"
            );
        }
    }

    fn git_add(dir: &Path, file: &str) {
        assert!(
            hermetic_git(&["add", file], dir).success(),
            "git add {file} failed"
        );
    }

    #[test]
    fn detect_real_git_flags_tracked_env_with_token() {
        // pass-35S review F3 (Gemini): integration test that
        // exercises the tracked lane via a real `git init` + `git add`.
        let td = TempDir::new().unwrap();
        init_git_repo(td.path());
        fs::write(
            td.path().join(".env"),
            "HTTP_BEARER_TOKEN=in-the-index\nFOO=bar\n",
        )
        .unwrap();
        git_add(td.path(), ".env");
        let findings = detect(td.path());
        let tracked: Vec<_> = findings.iter().filter(|f| f.tracked_by_git).collect();
        assert_eq!(
            tracked.len(),
            1,
            "tracked .env must produce exactly one finding via real git"
        );
        assert!(tracked[0].path.ends_with(".env"));
        assert!(
            tracked[0].matched_pattern.contains("HTTP_BEARER_TOKEN"),
            "pattern label must be the matched key"
        );
    }

    #[test]
    fn detect_real_git_skips_tracked_without_token_shape() {
        let td = TempDir::new().unwrap();
        init_git_repo(td.path());
        // Tracked .env with no token shape — must not be flagged.
        fs::write(td.path().join(".env"), "NODE_ENV=production\n").unwrap();
        git_add(td.path(), ".env");
        let findings = detect(td.path());
        assert!(
            findings.iter().all(|f| !f.tracked_by_git),
            "no tracked findings expected when content has no token shape"
        );
    }

    #[test]
    fn detect_works_from_repo_subdirectory() {
        // pass-35S review F1 (Gemini): the previous `.git`-presence
        // guard short-circuited the tracked lane when `project_root`
        // was a subdirectory of a git checkout. After the fix,
        // `list_tracked_files` defers to `git ls-files` exit status,
        // which works from any subdir inside a checkout.
        //
        // pass-35T review F1 strengthened this test to assert the
        // full detect() pipeline end-to-end, not just the helper.
        // `git ls-files` from a subdirectory returns paths relative
        // to CWD (verified in pass-35T fresh-eyes round-2), so
        // `project_root.join(rel)` resolves correctly.
        let td = TempDir::new().unwrap();
        init_git_repo(td.path());
        let sub = td.path().join("nested");
        fs::create_dir(&sub).unwrap();
        // Track an env file inside the subdir. `git add` is invoked
        // from the repo root with the relative path.
        fs::write(sub.join(".env"), "HTTP_BEARER_TOKEN=in-subdir\n").unwrap();
        git_add(td.path(), "nested/.env");

        // `list_tracked_files(sub)` returns `Some(_)` and enumerates
        // the tracked entry.
        let tracked = list_tracked_files(&sub)
            .expect("list_tracked_files must succeed from a subdirectory of a git checkout");
        assert!(
            tracked.iter().any(|e| e.ends_with(".env")),
            "ls-files from subdir must enumerate index entries"
        );

        // End-to-end: detect(sub) emits a tracked finding for the
        // subdir's .env (because `git ls-files` paths are relative
        // to CWD, `sub.join(".env")` is the correct absolute path).
        let findings = detect(&sub);
        let tracked_findings: Vec<_> = findings.iter().filter(|f| f.tracked_by_git).collect();
        assert_eq!(
            tracked_findings.len(),
            1,
            "detect() from subdir must surface the tracked .env"
        );
        assert!(tracked_findings[0].path.ends_with(".env"));
        assert!(
            tracked_findings[0]
                .matched_pattern
                .contains("HTTP_BEARER_TOKEN"),
        );
    }

    #[test]
    fn fixer_re_stats_and_refuses_to_widen_a_tightened_file() {
        // pass-35T review F3 (Codex + Gemini): if the file is
        // tightened between detect() and fix() (e.g., operator
        // ran `chmod 0o400` manually), the fixer MUST re-stat and
        // refuse to broaden it to 0o600. The detect-time snapshot
        // alone is unsafe.
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o644);
        // Construct a finding with the stale 0o644 mode snapshot,
        // then tighten the file before calling fix().
        let stale = CommittedEnvFileFinding {
            path: p.clone(),
            tracked_by_git: false,
            current_mode: 0o100644,
            matched_pattern: "HTTP_BEARER_TOKEN".into(),
        };
        fs::set_permissions(&p, fs::Permissions::from_mode(0o400)).unwrap();
        let ctx = ctx_for(&td, "2026-05-15T09-31-00Z__restat");
        let outcome = fix(&ctx, &stale).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // Mode must remain 0o400 — fixer must not have broadened it.
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o400,
            "fixer must NOT widen a tightened-since-detect file"
        );
    }

    #[test]
    fn fixer_re_stats_and_skips_vanished_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".env");
        write_with_mode(&p, "HTTP_BEARER_TOKEN=x", 0o644);
        let finding = detect(td.path()).into_iter().next().expect("finding");
        // Remove the file between detect and fix.
        fs::remove_file(&p).unwrap();
        let ctx = ctx_for(&td, "2026-05-15T09-31-01Z__vanish");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
