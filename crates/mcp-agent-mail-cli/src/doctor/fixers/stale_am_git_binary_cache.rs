//! `fm-environment_toolchain-stale-am-git-binary-cache` — P2.
//!
//! **Subsystem**: environment_toolchain.
//!
//! ## What's broken
//!
//! `mcp_agent_mail_core::git_binary` caches the resolved `git`
//! binary path + version for 24h. The doctor process shares that
//! cache. Between validation and TTL expiry, the operator can
//! swap the binary out from under us — `apt upgrade git`,
//! direnv flips `AM_GIT_BINARY`, a symlink retargets, etc. The
//! cached `ResolvedGitBinary` keeps reporting the stale path /
//! version until the TTL elapses.
//!
//! ## Detection (pure)
//!
//! 1. `peek_cached_resolution()` snapshots the process-wide cache.
//! 2. For each cached path, compare live disk state:
//!    - Missing → `missing` staleness.
//!    - Symlink with dangling target → `dangling_symlink`.
//!    - Symlink whose target's SHA-256 differs from
//!      `validated_sha` → `symlink_target_swap`.
//!    - Regular file whose SHA-256 differs from `validated_sha` →
//!      `binary_swap_in_place`.
//! 3. Cached entries without `validated_sha` (legacy entries
//!    created before the field existed) are skipped — we cannot
//!    distinguish "swap" from "first time we're recording the
//!    hash" so silence is the safe default.
//!
//! ## Fix
//!
//! **Detect-only.** The proper auto-fix is "clear the cache so
//! the next `resolve_git_binary` call refreshes" — but that
//! requires either reaching across crate boundaries to mutate
//! `CACHE` (which is intentionally private), or a public
//! `clear_cached_resolution()` helper that doesn't exist yet.
//! Operators get manual remediation: restart the host process
//! (which clears the OnceLock) or wait for the 24h TTL.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_core::git_binary::{ResolvedGitBinary, peek_cached_resolution};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-environment_toolchain-stale-am-git-binary-cache";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "environment_toolchain";

/// SHA-256 streaming chunk size. 64 KiB matches the cache-side
/// computation in `git_binary::sha256_of_path`.
const HASH_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StalenessKind {
    /// Cached path no longer exists on disk.
    Missing,
    /// Cached path is a symlink whose target is missing.
    DanglingSymlink,
    /// Cached path is a symlink; target exists but its bytes
    /// changed (different SHA-256 than what was validated).
    SymlinkTargetSwap,
    /// Cached path is a regular file; its bytes changed
    /// (different SHA-256 than what was validated).
    BinarySwapInPlace,
    /// Cached path exists but the doctor process couldn't
    /// read it (permission flipped, ACL denied, etc.). The
    /// stale-cache invariant can't be verified — the operator
    /// should investigate. pass-35AA review F1 (Gemini P2):
    /// previously this case was silently swallowed and gave a
    /// false clean bill of health.
    Unreadable,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleGitBinaryCacheFinding {
    pub cached_path: PathBuf,
    pub cached_version: String,
    pub cached_source: &'static str,
    pub cached_validated_age_secs: u64,
    pub staleness_kind: StalenessKind,
}

impl StaleGitBinaryCacheFinding {
    pub fn to_finding(&self) -> super::Finding {
        let kind_str = match self.staleness_kind {
            StalenessKind::Missing => "missing",
            StalenessKind::DanglingSymlink => "dangling_symlink",
            StalenessKind::SymlinkTargetSwap => "symlink_target_swap",
            StalenessKind::BinarySwapInPlace => "binary_swap_in_place",
            StalenessKind::Unreadable => "unreadable",
        };
        let title = format!(
            "cached git binary at {} is stale: {kind_str} (cached version {}, age {}s, source {})",
            self.cached_path.display(),
            self.cached_version,
            self.cached_validated_age_secs,
            self.cached_source,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "cached_path": self.cached_path.to_string_lossy(),
                "cached_version": self.cached_version,
                "cached_source": self.cached_source,
                "cached_validated_age_secs": self.cached_validated_age_secs,
                "staleness_kind": kind_str,
                "manual_remediation": {
                    "steps": [
                        "Restart the host process (`mcp-agent-mail serve` or `am serve`). The OnceLock cache is process-wide; a fresh process re-resolves the git binary.",
                        "Alternatively, wait for the 24h cache TTL to elapse (configurable via AM_GIT_BINARY_CACHE_SECS).",
                        "If AM_GIT_BINARY points at the swapped binary, unset it and let the resolver fall back to PATH.",
                    ],
                    "note": "Auto-clearing the process-wide cache from the doctor is intentionally not implemented in this first cut (would need a public `clear_cached_resolution()` helper).",
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Inputs the detector accepts. Production callers pass a `None`
/// override; tests inject a fabricated `ResolvedGitBinary` to
/// exercise each staleness branch without touching the
/// process-wide cache.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    pub cache_override: Option<ResolvedGitBinary>,
}

/// Detector. PURE — reads disk and the in-process cache snapshot.
pub fn detect(inputs: &DetectInputs) -> Vec<StaleGitBinaryCacheFinding> {
    let cached = match inputs.cache_override.clone() {
        Some(c) => c,
        None => match peek_cached_resolution() {
            Some(c) => c,
            None => return Vec::new(),
        },
    };
    // Legacy cache entries (validated_sha is None) can't be
    // compared. Skip silently — the next TTL refresh repopulates
    // the field.
    let Some(cached_sha) = cached.validated_sha else {
        return Vec::new();
    };
    let age = cached.validated_at.elapsed().as_secs();
    let version = cached.version.to_string();
    let source = cached.source;

    let lmeta = match fs::symlink_metadata(&cached.path) {
        Ok(m) => m,
        Err(_) => {
            return vec![StaleGitBinaryCacheFinding {
                cached_path: cached.path,
                cached_version: version,
                cached_source: source,
                cached_validated_age_secs: age,
                staleness_kind: StalenessKind::Missing,
            }];
        }
    };

    if lmeta.file_type().is_symlink() {
        match fs::canonicalize(&cached.path) {
            Ok(real) => match sha256_of_path(&real) {
                Some(live_sha) if live_sha == cached_sha => {}
                Some(_) => {
                    return vec![StaleGitBinaryCacheFinding {
                        cached_path: cached.path,
                        cached_version: version,
                        cached_source: source,
                        cached_validated_age_secs: age,
                        staleness_kind: StalenessKind::SymlinkTargetSwap,
                    }];
                }
                // Symlink target exists (canonicalize succeeded)
                // but we couldn't hash it (permissions, ACL,
                // etc.). pass-35AA review F1 (Gemini): surface
                // this rather than silently skipping.
                None => {
                    return vec![StaleGitBinaryCacheFinding {
                        cached_path: cached.path,
                        cached_version: version,
                        cached_source: source,
                        cached_validated_age_secs: age,
                        staleness_kind: StalenessKind::Unreadable,
                    }];
                }
            },
            Err(_) => {
                return vec![StaleGitBinaryCacheFinding {
                    cached_path: cached.path,
                    cached_version: version,
                    cached_source: source,
                    cached_validated_age_secs: age,
                    staleness_kind: StalenessKind::DanglingSymlink,
                }];
            }
        }
    } else {
        match sha256_of_path(&cached.path) {
            Some(live_sha) if live_sha == cached_sha => {}
            Some(_) => {
                return vec![StaleGitBinaryCacheFinding {
                    cached_path: cached.path,
                    cached_version: version,
                    cached_source: source,
                    cached_validated_age_secs: age,
                    staleness_kind: StalenessKind::BinarySwapInPlace,
                }];
            }
            // Existing file but unreadable — surface explicitly
            // (pass-35AA review F1, Gemini P2).
            None => {
                return vec![StaleGitBinaryCacheFinding {
                    cached_path: cached.path,
                    cached_version: version,
                    cached_source: source,
                    cached_validated_age_secs: age,
                    staleness_kind: StalenessKind::Unreadable,
                }];
            }
        }
    }

    Vec::new()
}

fn sha256_of_path(path: &Path) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut f = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    use std::io::Read;
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.finalize().into())
}

/// Fixer. Detect-only — manual remediation only.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &StaleGitBinaryCacheFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::git_binary::GitVersion;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    use tempfile::TempDir;

    fn make_executable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn cache_entry_for(path: PathBuf, sha: Option<[u8; 32]>) -> ResolvedGitBinary {
        ResolvedGitBinary {
            path,
            version: GitVersion::new(2, 50, 1),
            validated_at: Instant::now(),
            source: "default",
            validated_sha: sha,
        }
    }

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): live binary
    /// matches the cached SHA → no finding.
    #[test]
    fn detector_skips_when_live_sha_matches_cached_sha() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("git");
        fs::write(&bin, b"#!/bin/sh\necho 'git version 2.50.1'\n").unwrap();
        make_executable(&bin);
        let sha = sha256_of_path(&bin).expect("must hash");
        let cache = cache_entry_for(bin.clone(), Some(sha));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty(), "matching SHAs must not emit a finding");
    }

    /// **NEGATIVE TEST**: legacy cache entries with no
    /// validated_sha can't be compared and must skip silently.
    #[test]
    fn detector_skips_legacy_cache_entry_with_none_sha() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("git");
        fs::write(&bin, b"#!/bin/sh\necho 'git version 2.50.1'\n").unwrap();
        make_executable(&bin);
        let cache = cache_entry_for(bin, None);
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty(), "None validated_sha must skip silently");
    }

    /// **NEGATIVE TEST**: cache empty → no finding.
    #[test]
    fn detector_skips_when_no_cache_entry() {
        let inputs = DetectInputs::default();
        let findings = detect(&inputs);
        // Production cache may be populated by other tests in this
        // process. Just assert detect() doesn't panic and returns
        // a Vec.
        // We can't strictly assert empty here without resetting
        // the global cache, which we don't want to do in tests.
        // So just exercise the no-override path.
        let _ = findings;
    }

    /// **POSITIVE**: cached path missing → Missing staleness.
    #[test]
    fn detector_flags_missing_path() {
        let cache = cache_entry_for(PathBuf::from("/nonexistent/path/to/git"), Some([0u8; 32]));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].staleness_kind, StalenessKind::Missing);
    }

    /// **POSITIVE**: cached binary bytes changed → BinarySwapInPlace.
    #[test]
    fn detector_flags_in_place_binary_swap() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("git");
        fs::write(&bin, b"#!/bin/sh\necho old\n").unwrap();
        make_executable(&bin);
        let sha = sha256_of_path(&bin).expect("must hash");
        // Now swap the bytes (simulate `apt upgrade git`).
        fs::write(&bin, b"#!/bin/sh\necho new totally different bytes\n").unwrap();
        let cache = cache_entry_for(bin, Some(sha));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].staleness_kind, StalenessKind::BinarySwapInPlace);
    }

    /// **POSITIVE**: cached path is a symlink whose target was
    /// retargeted → SymlinkTargetSwap.
    #[test]
    fn detector_flags_symlink_target_swap() {
        let td = TempDir::new().unwrap();
        let real_a = td.path().join("git-a");
        fs::write(&real_a, b"#!/bin/sh\necho a\n").unwrap();
        make_executable(&real_a);
        let real_b = td.path().join("git-b");
        fs::write(&real_b, b"#!/bin/sh\necho b totally different\n").unwrap();
        make_executable(&real_b);
        let link = td.path().join("git");
        std::os::unix::fs::symlink(&real_a, &link).unwrap();
        // Hash the symlink's target (real_a) as the cached SHA.
        let sha = sha256_of_path(&real_a).expect("must hash");
        // Retarget the symlink to a different file.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&real_b, &link).unwrap();
        let cache = cache_entry_for(link, Some(sha));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].staleness_kind, StalenessKind::SymlinkTargetSwap);
    }

    /// **POSITIVE** (pass-35AA review F1, Gemini): cached path
    /// exists but the doctor can't read it (chmod 0o000) — the
    /// SHA-256 comparison must surface this as `Unreadable`
    /// rather than silently giving a clean bill of health.
    #[test]
    fn detector_flags_unreadable_existing_file() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("git");
        fs::write(&bin, b"#!/bin/sh\necho old\n").unwrap();
        // Make the file completely unreadable so File::open
        // fails. (Skip the assertion on the OS or environment
        // we run as root, where 0o000 is still readable.)
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o000)).unwrap();
        let opened = fs::File::open(&bin).is_ok();
        if opened {
            // Test environment runs as root or the test process
            // can otherwise bypass 0o000 — skip the assertion
            // rather than flake.
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let cache = cache_entry_for(bin.clone(), Some([0xAB; 32]));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        // Restore permissions so the tempdir teardown can
        // remove the file.
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).ok();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].staleness_kind, StalenessKind::Unreadable);
    }

    /// **POSITIVE**: cached path is a symlink that became
    /// dangling.
    #[test]
    fn detector_flags_dangling_symlink() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("git-real");
        fs::write(&real, b"old").unwrap();
        let link = td.path().join("git");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let sha = [0xAB; 32]; // arbitrary; the target gets unlinked
        // Remove the target so canonicalize() fails.
        fs::remove_file(&real).unwrap();
        let cache = cache_entry_for(link, Some(sha));
        let inputs = DetectInputs {
            cache_override: Some(cache),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].staleness_kind, StalenessKind::DanglingSymlink);
    }

    #[test]
    fn finding_serializes_with_kind_and_remediation() {
        let f = StaleGitBinaryCacheFinding {
            cached_path: "/usr/bin/git".into(),
            cached_version: "2.50.1".into(),
            cached_source: "default",
            cached_validated_age_secs: 3600,
            staleness_kind: StalenessKind::BinarySwapInPlace,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"staleness_kind\":\"binary_swap_in_place\""));
        assert!(s.contains("\"cached_validated_age_secs\":3600"));
        assert!(s.contains("manual_remediation"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = StaleGitBinaryCacheFinding {
            cached_path: "/x".into(),
            cached_version: "2.0.0".into(),
            cached_source: "default",
            cached_validated_age_secs: 0,
            staleness_kind: StalenessKind::Missing,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
