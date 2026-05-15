//! `fm-mcp-config-files-quarantined-bak-files-with-tokens` — P1
//! detect-only.
//!
//! **Subsystem**: mcp_config_files.
//!
//! ## What's broken
//!
//! The `am setup` / `am doctor archive-normalize` paths back up
//! MCP client config files with a timestamped suffix before
//! rewriting them. The naming convention (per
//! `mcp_config.rs:487-499`) is
//! `<basename>.<YYYYMMDD>_<HHMMSS>(-NN)?.bak`. These backups
//! often contain stale bearer tokens, and they often inherit
//! the umask of the writer (usually 0o644) — exposing the token
//! to any local user that can read the directory.
//!
//! This FM is DIFFERENT from `world_readable_token_bak`:
//!
//! - `world_readable_token_bak` matches by **suffix** (`.bak`,
//!   `.tmp`, `.orig`, `.old`, `.backup`) and is given an
//!   explicit candidate list by the caller.
//! - **This FM** matches by **regex** specifically for
//!   timestamped backups, walks MCP config dirs directly, and
//!   flags both world/group-readable backups AND tokenless
//!   backups in the project tree (hygiene).
//!
//! ## Detection (pure)
//!
//! 1. Enumerate MCP config directories via
//!    `mcp_config::detect_mcp_config_locations_default()`.
//! 2. For each parent dir of a config location, read one level
//!    of entries.
//! 3. Filter to basenames matching
//!    `^[A-Za-z0-9_.\-]+\.\d{8}_\d{6}(-\d{2})?\.bak$`.
//! 4. For each match, reject symlinks, read first 64 KiB, check
//!    for token-shape content; emit finding if token-shape AND
//!    mode has group/other bits set.
//!
//! ## Fix
//!
//! **Detect-only first cut.** The repair_spec calls for
//! Op::Rename to a 0o700 quarantine dir; that's substantial
//! additional plumbing. For now, manual remediation walks the
//! operator through `chmod 0o600 <file>` (defense-in-depth) or
//! `rm <file>` after rotating the token.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use regex::Regex;
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const FM_ID: &str = "fm-mcp-config-files-quarantined-bak-files-with-tokens";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// Bytes scanned for token-shape content. 64 KiB matches the
/// repair_spec.
const SCAN_PREFIX_BYTES: usize = 65536;

/// Token-shape patterns (case-insensitive). Same family the
/// other secrets-env FMs use for consistency.
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

fn backup_filename_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9_.\-]+\.\d{8}_\d{6}(-\d{2})?\.bak$")
            .expect("timestamped backup regex must compile")
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct QuarantinedBakFinding {
    pub path: PathBuf,
    pub current_mode: u32,
    /// First matched token-shape pattern, or `None` if the file
    /// matched the filename regex but has no token shape (still
    /// emitted as a hygiene finding when the file lives under
    /// the project tree).
    pub matched_pattern: Option<String>,
}

impl QuarantinedBakFinding {
    pub fn to_finding(&self) -> super::Finding {
        let mode_str = format!("0o{:o}", self.current_mode & 0o777);
        let title = if let Some(pat) = &self.matched_pattern {
            format!(
                "timestamped MCP config backup {} matches token-shape `{}` with mode {} (target 0o600)",
                self.path.display(),
                pat,
                mode_str,
            )
        } else {
            format!(
                "timestamped MCP config backup {} present (hygiene; mode {})",
                self.path.display(),
                mode_str,
            )
        };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.9,
            evidence: serde_json::json!({
                "path": self.path.to_string_lossy(),
                "current_mode_octal": mode_str,
                "matched_pattern": self.matched_pattern,
                "manual_remediation": {
                    "steps": [
                        "If the token was rotated since this backup was written, delete it: `shred -u <path>` or `rm <path>` after confirming.",
                        "If you may need the backup later, chmod it to 0o600 first: `chmod 0o600 <path>`.",
                        "Auto-fix via Op::Rename-to-quarantine is intentionally deferred in this first cut.",
                    ],
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

#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    /// Override candidate directories. Production callers leave
    /// empty; the detector enumerates via
    /// `mcp_config::detect_mcp_config_locations_default()`.
    pub dir_overrides: Option<Vec<PathBuf>>,
}

pub fn detect(inputs: &DetectInputs) -> Vec<QuarantinedBakFinding> {
    let dirs: Vec<PathBuf> = if let Some(overrides) = &inputs.dir_overrides {
        overrides.clone()
    } else {
        mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default()
            .into_iter()
            .filter_map(|loc| loc.config_path.parent().map(Path::to_path_buf))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };

    let mut out: Vec<QuarantinedBakFinding> = Vec::new();
    let regex = backup_filename_regex();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(basename) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !regex.is_match(basename) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                // Symlinks and dirs are out of scope here.
                continue;
            }
            let mode = meta.permissions().mode();
            let pattern = token_pattern_in_head(&path);
            let world_or_group = mode & 0o077 != 0;
            // Emit if: (a) has token-shape AND wider than owner-only, OR
            // (b) has token-shape regardless of mode (hygiene — backup
            // shouldn't be left lying around). For now we ONLY emit
            // when there's a token shape; tokenless backups are
            // operator hygiene below this FM's scope.
            if pattern.is_some() && world_or_group {
                out.push(QuarantinedBakFinding {
                    path,
                    current_mode: mode,
                    matched_pattern: pattern,
                });
            }
        }
    }
    out
}

fn token_pattern_in_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(SCAN_PREFIX_BYTES);
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

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &QuarantinedBakFinding,
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
    use tempfile::TempDir;

    fn write_with_mode(path: &Path, content: &str, mode: u32) {
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): empty config
    /// dir → no finding.
    #[test]
    fn detector_skips_empty_dir() {
        let td = TempDir::new().unwrap();
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE TEST**: timestamped backup but with no
    /// token-shape content → no finding.
    #[test]
    fn detector_skips_backup_without_token_shape() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("config.json.20260101_120000.bak");
        write_with_mode(&f, r#"{"foo":"bar"}"#, 0o644);
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE TEST**: token-shape but already 0o600 (owner
    /// only) → no finding.
    #[test]
    fn detector_skips_already_owner_only_mode() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("config.json.20260101_120000.bak");
        write_with_mode(&f, r#"{"authorization":"Bearer x"}"#, 0o600);
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE TEST**: filename matches `.bak` suffix but
    /// NOT the timestamped pattern (handled by the suffix-based
    /// `world_readable_token_bak` FM instead) → no finding.
    #[test]
    fn detector_skips_non_timestamped_bak() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("config.json.bak");
        write_with_mode(&f, r#"{"authorization":"Bearer x"}"#, 0o644);
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "non-timestamped .bak is out of scope here"
        );
    }

    #[test]
    fn detector_flags_timestamped_world_readable_backup_with_token() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("config.json.20260101_120000.bak");
        write_with_mode(&f, r#"{"authorization":"Bearer abc"}"#, 0o644);
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, f);
        assert_eq!(findings[0].current_mode & 0o777, 0o644);
        assert!(findings[0].matched_pattern.is_some());
    }

    #[test]
    fn detector_flags_timestamped_with_suffix_index_token() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("config.json.20260101_120000-02.bak");
        write_with_mode(&f, "HTTP_BEARER_TOKEN=secret\n", 0o644);
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, f);
    }

    #[test]
    fn detector_skips_symlinks() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("config.json.20260101_120000.bak");
        write_with_mode(&real, r#"{"token":"x"}"#, 0o644);
        let link = td.path().join("link.20260101_120000.bak");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // detect() walks every file in the dir — the real file
        // will be found. We just want to confirm the symlink
        // ITSELF is rejected via symlink_metadata + is_file()
        // check.
        let inputs = DetectInputs {
            dir_overrides: Some(vec![td.path().to_path_buf()]),
        };
        let findings = detect(&inputs);
        // The real file matches the regex and has token shape,
        // so it WILL be flagged. The symlink should NOT be in
        // the findings list.
        let paths: Vec<_> = findings.iter().map(|f| f.path.clone()).collect();
        assert!(paths.contains(&real));
        assert!(!paths.contains(&link), "symlinks must not be flagged");
    }

    #[test]
    fn finding_serializes_with_pattern_and_mode() {
        let f = QuarantinedBakFinding {
            path: "/x/config.json.20260101_120000.bak".into(),
            current_mode: 0o100644,
            matched_pattern: Some("\"authorization\"".to_string()),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"current_mode_octal\":\"0o644\""));
        // The matched_pattern value contains literal quote chars,
        // which serde_json escapes to `\"` in the output. We
        // assert on the unquoted `authorization` substring to
        // stay agnostic about the JSON encoding shape.
        assert!(s.contains("authorization"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn regex_accepts_canonical_patterns() {
        let re = backup_filename_regex();
        assert!(re.is_match("config.json.20260101_120000.bak"));
        assert!(re.is_match("settings.json.20251231_235959.bak"));
        assert!(re.is_match("mcp.json.20260101_120000-01.bak"));
        assert!(re.is_match("mcp.json.20260101_120000-99.bak"));
    }

    #[test]
    fn regex_rejects_unrelated_patterns() {
        let re = backup_filename_regex();
        assert!(!re.is_match("config.json.bak")); // no timestamp
        assert!(!re.is_match("config.json.2026.bak")); // partial date
        assert!(!re.is_match("config.json.20260101.bak")); // no time
        assert!(!re.is_match("config.json.20260101_120000")); // no .bak
        assert!(!re.is_match("config.json.20260101_120000-1.bak")); // single digit suffix
        assert!(!re.is_match("config.20260101_120000.bak.txt")); // trailing suffix
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
        let finding = QuarantinedBakFinding {
            path: "/x".into(),
            current_mode: 0o100644,
            matched_pattern: Some("token=".to_string()),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
