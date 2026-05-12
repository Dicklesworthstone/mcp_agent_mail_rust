//! `fm-secrets_env_state-bak-tokens-readable` — P1.
//!
//! **Subsystem**: secrets_env_state (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! The MCP agent-mail installer and the `am doctor repair` /
//! `am doctor archive-normalize` paths sometimes leave `.bak`, `.tmp`,
//! `.backup`, `.orig`, or `.old` files alongside config files when they
//! rewrite tokens or URLs. Example:
//! `~/.codex/config.toml.20260321_155159.bak`
//! containing the previous bearer token. If the backup is left with
//! the same permission bits as the original (often 0o644 — group-
//! and world-readable on most installs), the token is exposed to any
//! local user account.
//!
//! The original config file is typically chmod'd to 0o600 by the
//! writer; the backup inherits 0o644 from the umask. So we look for
//! `.bak` / `.tmp` / `.backup` / `.orig` / `.old` files in known MCP config
//! directories
//! that:
//! 1. Contain token-shape content (a long base64-ish string or a
//!    JSON key like `"authorization"` / `"bearer"` / `"token"`).
//! 2. Have permission bits broader than 0o600.
//!
//! ## Fix (`Op::Chmod` — new pattern)
//!
//! `mutate(ctx, path, Op::Chmod { mode: 0o600 })`. Pass-3's G4 fix
//! made `Op::Chmod` use `chmod_via_fd` with `O_NOFOLLOW`, so this
//! fixer is symlink-safe.
//!
//! Demonstrates the third write-shape pattern in the fixer suite:
//! - Passes 8-10: `Op::Rename` quarantine
//! - Pass 11: detect-only (no write at all)
//! - **Pass 12: `Op::Chmod` (metadata-only mutation)** ← this
//!
//! ## Reversibility
//!
//! `am doctor undo <run-id>` reads `actions.jsonl`'s `before_mode`
//! field and restores the file's original mode. Pass-1 wired the
//! before_mode/after_mode capture; pass-12 exercises it.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-secrets_env_state-bak-tokens-readable";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "secrets_env_state";

/// Safe permission mode: rw for owner only.
pub const SAFE_MODE: u32 = 0o600;

/// Token-shape patterns. Detector matches any of these in the file
/// body (case-insensitive substring). Updated to whatever the project
/// rotates next.
const TOKEN_SHAPE_PATTERNS: &[&str] = &[
    "\"authorization\"",
    "\"bearer\"",
    "\"token\"",
    "authorization=",
    "bearer=",
    "token=",
    "HTTP_BEARER_TOKEN",
];

/// Suffixes treated as backup files. We only flag these — the live
/// config file's permissions are the writer's responsibility.
///
/// `pub` so callers (e.g., `doctor::default_token_backup_candidates`)
/// can enumerate candidates from this single source of truth instead
/// of duplicating the list. Drift between the detector's accept-set
/// and the handler's enumeration-set is the kind of bug that quietly
/// leaves a real `.backup` token-bearing file out of every `--only`
/// run forever — promoting this `const` to `pub` makes that
/// structurally impossible.
pub const BACKUP_SUFFIX_HINTS: &[&str] = &[".bak", ".tmp", ".backup", ".orig", ".old"];

#[derive(Debug, Clone, Serialize)]
pub struct WorldReadableTokenBakFinding {
    pub path: PathBuf,
    pub current_mode: u32,
    /// First token-shape pattern matched in the body (for evidence).
    pub matched_pattern: String,
}

impl WorldReadableTokenBakFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "token-bearing backup {} has world-readable mode 0o{:o} (target: 0o600)",
            self.path.display(),
            self.current_mode & 0o777,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "path": self.path.to_string_lossy(),
                // Render mode as octal for operator readability.
                "current_mode_octal": format!("0o{:o}", self.current_mode & 0o777),
                "current_mode_decimal": self.current_mode,
                "target_mode_octal": format!("0o{:o}", SAFE_MODE),
                "matched_pattern": self.matched_pattern,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {} --yes", FM_ID),
                explain_command: format!("am doctor explain {}", FM_ID),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE.
///
/// `candidate_paths` is the list of files to scan. Caller is
/// responsible for enumerating MCP config directories (e.g.,
/// `~/.codex/`, `~/.claude/`, `~/.config/mcp-agent-mail/`) and
/// passing backup-suffixed files to this function.
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<WorldReadableTokenBakFinding> {
    let mut out = Vec::new();
    for path in candidate_paths {
        // Only consider files with a backup-suffix hint. (Caller may
        // be paranoid; we re-check to keep the detector self-contained.)
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !BACKUP_SUFFIX_HINTS.iter().any(|s| name.ends_with(s)) {
            continue;
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_file() {
            continue; // symlink-attack defense
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            // Already 0o600 (or 0o400, 0o700, etc. with no group/other bits).
            continue;
        }
        // Read body and look for token-shape pattern.
        let body = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let lowered = body.to_lowercase();
        let matched = TOKEN_SHAPE_PATTERNS
            .iter()
            .find(|pat| lowered.contains(&pat.to_lowercase()));
        let Some(pat) = matched else {
            continue;
        };
        out.push(WorldReadableTokenBakFinding {
            path: path.clone(),
            current_mode: mode,
            matched_pattern: (*pat).to_string(),
        });
    }
    out
}

/// Fixer. Routes through `mutate()` with `Op::Chmod`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &WorldReadableTokenBakFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if !finding.path.exists() {
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
        quarantined_paths: Vec::new(), // Chmod doesn't quarantine
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
            run_dir: run_dir.clone(),
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

    fn write_with_mode(path: &std::path::Path, content: &str, mode: u32) {
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn detector_returns_empty_for_safe_mode() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml.bak");
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o600);
        let findings = detect(&[p]);
        assert!(findings.is_empty(), "0o600 file must NOT be flagged");
    }

    #[test]
    fn detector_flags_world_readable_token_bak() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml.bak");
        write_with_mode(&p, r#"{"token":"abcdef"}"#, 0o644);
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, p);
        assert_eq!(findings[0].current_mode & 0o777, 0o644);
        assert!(findings[0].matched_pattern.contains("token"));
    }

    #[test]
    fn detector_flags_bearer_authorization_patterns() {
        let td = TempDir::new().unwrap();
        for (suffix, body) in [
            (".bak", r#"{"authorization":"Bearer xyz"}"#),
            (".tmp", "HTTP_BEARER_TOKEN=secret123"),
            (".backup", r#"{"bearer":"abc"}"#),
            (".orig", "authorization=Bearer 42"),
            (".old", r#"{"token":"abc"}"#),
        ] {
            let p = td.path().join(format!("file{suffix}"));
            write_with_mode(&p, body, 0o644);
            let findings = detect(std::slice::from_ref(&p));
            assert_eq!(
                findings.len(),
                1,
                "{suffix} should be flagged for body {body}"
            );
            fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn detector_skips_non_backup_suffixed_files() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml"); // no .bak
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o644);
        let findings = detect(&[p]);
        assert!(
            findings.is_empty(),
            "live config files must not be flagged here"
        );
    }

    #[test]
    fn detector_requires_backup_hint_as_suffix() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.bakery");
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o644);
        let findings = detect(&[p]);
        assert!(
            findings.is_empty(),
            "backup hints are suffixes, not arbitrary substrings"
        );
    }

    #[test]
    fn detector_skips_no_token_shape() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("README.bak");
        write_with_mode(&p, "ordinary backup with no secrets", 0o644);
        let findings = detect(&[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_refuses_symlink() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.bak");
        write_with_mode(&real, r#"{"token":"abc"}"#, 0o644);
        let link = td.path().join("link.bak");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let findings = detect(&[link]);
        assert!(findings.is_empty(), "symlink target must not be followed");
    }

    #[test]
    fn finding_serializes_with_octal_mode_render() {
        let f = WorldReadableTokenBakFinding {
            path: "/x/config.toml.bak".into(),
            current_mode: 0o100644,
            matched_pattern: "\"token\"".into(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"current_mode_octal\":\"0o644\""));
        assert!(s.contains("\"target_mode_octal\":\"0o600\""));
        assert!(g.title.contains("0o644"));
    }

    #[test]
    fn fixer_chmods_to_0o600_via_mutate() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml.bak");
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o644);
        let findings = detect(std::slice::from_ref(&p));
        let run_id = "2026-05-11T07-00-00Z__chmod";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        let new_mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(new_mode & 0o777, 0o600, "fixer must chmod to 0o600");
        // Quarantine list is empty for Chmod ops.
        assert!(outcome.quarantined_paths.is_empty());
    }

    #[test]
    fn fixer_idempotent_on_already_safe_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml.bak");
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o600);
        // Detector returns empty for safe files; if a caller still calls
        // fix() with a stale finding, mutate runs but the file is already
        // safe — no observable change.
        let finding = WorldReadableTokenBakFinding {
            path: p.clone(),
            current_mode: 0o100600,
            matched_pattern: "\"token\"".into(),
        };
        let ctx = ctx_for(&td, "2026-05-11T07-00-01Z__idem");
        let _ = fix(&ctx, &finding).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn fixer_then_undo_restores_original_mode() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml.bak");
        write_with_mode(&p, r#"{"token":"abc"}"#, 0o644);
        let findings = detect(std::slice::from_ref(&p));
        let run_id = "2026-05-11T07-00-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        // Now 0o600.
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(ctx);
        // Undo should restore to 0o644.
        let summary = crate::doctor::undo::run_undo(td.path(), run_id, false, true).expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        let restored = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            restored & 0o777,
            0o644,
            "undo must restore original 0o644 mode"
        );
    }
}
