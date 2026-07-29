//! `fm-environment_toolchain-path-order-shadows-am` — P1.
//!
//! **Subsystem**: environment_toolchain.
//!
//! ## What's broken
//!
//! Multiple distinct `am` executables are present on `PATH`,
//! resolving to different physical files. The shell's first-
//! match wins: if a stale Python `am` (or another agent's local
//! development build) appears in `PATH` before the canonical
//! Rust binary at `~/.local/bin/am`, every interactive operator
//! command runs against the wrong binary.
//!
//! Concrete impact:
//! - Stale Python `am` against a Rust-format DB → schema /
//!   timestamp errors (see
//!   `fm-db-state-files-text-timestamp-contamination`).
//! - Different-version Rust `am` → divergent flag surfaces
//!   (commands the operator expects to exist may be missing).
//! - Operator's `which am` says one path; their `am --version`
//!   reports another (because either is shadowed by aliases or
//!   alternative resolution).
//!
//! ## Detection (pure function)
//!
//! Iterate `PATH` entries in order. For each, check for an
//! `am` (or `am.exe` on Windows-like targets) executable.
//! `canonicalize` to resolve symlinks. Group hits by canonical
//! path. If more than one distinct canonical path appears, emit
//! a finding listing every hit with its PATH-order index and
//! resolved canonical target.
//!
//! Symlink chains pointing at the same final file are NOT
//! flagged (the operator's setup may intentionally use
//! `~/.local/bin/am -> /opt/am/bin/am` for stable upgrades).
//!
//! ## Fix
//!
//! **Detect-only.** Editing shell rc / PATH is explicitly out
//! of scope for the doctor surface — `safety_envelope.md` lists
//! shell rc files as a no-touch category. Manual remediation
//! enumerates each hit so the operator can decide which to
//! remove or reorder.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-environment_toolchain-path-order-shadows-am";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "environment_toolchain";

#[derive(Debug, Clone, Serialize)]
pub struct AmHit {
    /// Position in `PATH` (0 = first entry).
    pub path_index: usize,
    /// The literal path that was probed
    /// (`<PATH-entry>/am`).
    pub probed: PathBuf,
    /// `canonicalize`'d resolution (symlinks followed).
    pub canonical: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathOrderShadowsAmFinding {
    /// Every `am` hit on PATH, in PATH order. At least 2
    /// distinct canonical paths (else no finding).
    pub hits: Vec<AmHit>,
    /// The first-match `am` — the binary the operator's shell
    /// actually invokes.
    pub winning_canonical: PathBuf,
    /// Distinct canonical-path count among hits. >1 always
    /// (the threshold for emitting a finding).
    pub distinct_canonical_count: usize,
}

impl PathOrderShadowsAmFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} distinct `am` binaries on PATH; first-match is {}",
            self.distinct_canonical_count,
            self.winning_canonical.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "hits": self.hits,
                "winning_canonical": self.winning_canonical.to_string_lossy(),
                "distinct_canonical_count": self.distinct_canonical_count,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only: shell rc / PATH edits out of
                // scope.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        let mut steps = vec![format!(
            "PATH contains {} distinct `am` binaries. Shell-first-match wins; \
             your current resolution is {}.",
            self.distinct_canonical_count,
            self.winning_canonical.display(),
        )];
        for h in &self.hits {
            steps.push(format!(
                "  PATH[{}] {} → canonical {}",
                h.path_index,
                h.probed.display(),
                h.canonical.display(),
            ));
        }
        steps.push(
            "Remediation: edit your shell rc (`.bashrc` / `.zshrc` / `.profile`) to either \
             remove the unwanted PATH entry or reorder so `~/.local/bin` precedes it. \
             `am doctor` will NOT auto-edit shell rc per safety policy."
                .to_string(),
        );
        steps.join("\n")
    }
}

/// Detector inputs. `path_env` can be overridden for tests.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    /// `PATH` env var value. `None` reads from
    /// `std::env::var("PATH")`.
    pub path_override: Option<String>,
}

#[cfg(unix)]
fn am_candidate_names() -> &'static [&'static str] {
    &["am"]
}

#[cfg(not(unix))]
fn am_candidate_names() -> &'static [&'static str] {
    &["am.exe", "am"]
}

/// Detector. PURE w.r.t. caller-supplied PATH; performs
/// filesystem stat + canonicalize calls but never writes.
pub fn detect(inputs: &DetectInputs) -> Vec<PathOrderShadowsAmFinding> {
    detect_with_candidate_names(inputs, am_candidate_names())
}

fn detect_with_candidate_names(
    inputs: &DetectInputs,
    candidate_names: &[&str],
) -> Vec<PathOrderShadowsAmFinding> {
    let path_var = inputs
        .path_override
        .clone()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    if path_var.is_empty() || candidate_names.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut seen_canonical: BTreeMap<PathBuf, ()> = BTreeMap::new();
    // Pass-35-review Gemini F3 / Codex F3 (P1): use
    // `std::env::split_paths` so the platform's canonical PATH
    // delimiter is honored (`:` on Unix, `;` on Windows). The
    // pre-fix hardcoded `:` would treat the whole Windows PATH
    // as one invalid entry.
    for (i, dir) in std::env::split_paths(&path_var).enumerate() {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for &candidate_name in candidate_names {
            let probed = dir.join(candidate_name);
            let meta = match std::fs::symlink_metadata(&probed) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Skip if the path itself isn't a file or symlink.
            let ftype = meta.file_type();
            if !ftype.is_file() && !ftype.is_symlink() {
                continue;
            }
            let canonical = match std::fs::canonicalize(&probed) {
                Ok(c) => c,
                Err(_) => continue, // dangling symlink — skip
            };
            // Skip non-executable (rare but possible).
            if !is_executable(&canonical) {
                continue;
            }
            seen_canonical.entry(canonical.clone()).or_insert(());
            hits.push(AmHit {
                path_index: i,
                probed,
                canonical,
            });
            // Command resolution picks the first matching executable candidate
            // within a PATH entry, then moves on to the next PATH entry.
            break;
        }
    }
    let distinct = seen_canonical.len();
    if distinct < 2 {
        return Vec::new();
    }
    let winning_canonical = hits[0].canonical.clone();
    vec![PathOrderShadowsAmFinding {
        hits,
        winning_canonical,
        distinct_canonical_count: distinct,
    }]
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    true
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &PathOrderShadowsAmFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn make_exec(p: &std::path::Path, content: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn detector_returns_empty_when_path_unset() {
        let inputs = DetectInputs {
            path_override: Some(String::new()),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_for_single_am_on_path() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("bin");
        make_exec(&bin.join("am"), b"#!/bin/sh\necho rust");
        let inputs = DetectInputs {
            path_override: Some(bin.to_string_lossy().into_owned()),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_two_distinct_am_binaries() {
        let td = TempDir::new().unwrap();
        let bin_a = td.path().join("a");
        let bin_b = td.path().join("b");
        make_exec(&bin_a.join("am"), b"#!/bin/sh\necho a");
        make_exec(&bin_b.join("am"), b"#!/bin/sh\necho b");
        let path_var = format!("{}:{}", bin_a.to_string_lossy(), bin_b.to_string_lossy());
        let inputs = DetectInputs {
            path_override: Some(path_var),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].distinct_canonical_count, 2);
        assert_eq!(findings[0].hits.len(), 2);
        assert_eq!(findings[0].hits[0].path_index, 0);
        assert_eq!(findings[0].hits[1].path_index, 1);
    }

    #[cfg(unix)]
    #[test]
    fn detector_candidate_names_can_find_windows_style_am_exe() {
        let td = TempDir::new().unwrap();
        let bin_a = td.path().join("a");
        let bin_b = td.path().join("b");
        make_exec(&bin_a.join("am.exe"), b"#!/bin/sh\necho a");
        make_exec(&bin_b.join("am.exe"), b"#!/bin/sh\necho b");
        let path_var = format!("{}:{}", bin_a.to_string_lossy(), bin_b.to_string_lossy());
        let inputs = DetectInputs {
            path_override: Some(path_var),
        };
        let findings = detect_with_candidate_names(&inputs, &["am.exe"]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].distinct_canonical_count, 2);
        assert_eq!(findings[0].hits.len(), 2);
        assert!(findings[0].hits.iter().all(|hit| {
            hit.probed.file_name().and_then(|name| name.to_str()) == Some("am.exe")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn detector_candidate_names_do_not_double_count_one_path_entry() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("bin");
        make_exec(&bin.join("am.exe"), b"#!/bin/sh\necho exe");
        make_exec(&bin.join("am"), b"#!/bin/sh\necho extensionless");
        let inputs = DetectInputs {
            path_override: Some(bin.to_string_lossy().into_owned()),
        };

        let findings = detect_with_candidate_names(&inputs, &["am.exe", "am"]);

        assert!(
            findings.is_empty(),
            "multiple candidate names in one PATH entry should resolve to one command hit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_collapses_symlinks_to_same_canonical() {
        // Two PATH entries; one is a symlink to the other. After
        // canonicalize, they point at the same file → NO finding.
        let td = TempDir::new().unwrap();
        let real_dir = td.path().join("real");
        let link_dir = td.path().join("link");
        make_exec(&real_dir.join("am"), b"#!/bin/sh\necho real");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
        let path_var = format!(
            "{}:{}",
            real_dir.to_string_lossy(),
            link_dir.to_string_lossy()
        );
        let inputs = DetectInputs {
            path_override: Some(path_var),
        };
        assert!(
            detect(&inputs).is_empty(),
            "symlinked-same-file must not flag"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_skips_non_executable_am() {
        let td = TempDir::new().unwrap();
        let bin_a = td.path().join("a");
        let bin_b = td.path().join("b");
        make_exec(&bin_a.join("am"), b"#!/bin/sh\necho a");
        // Non-executable file at b/am.
        fs::create_dir_all(&bin_b).unwrap();
        fs::write(bin_b.join("am"), b"not exec").unwrap();
        let path_var = format!("{}:{}", bin_a.to_string_lossy(), bin_b.to_string_lossy());
        let inputs = DetectInputs {
            path_override: Some(path_var),
        };
        assert!(detect(&inputs).is_empty(), "non-exec must not count");
    }

    #[test]
    fn detector_skips_empty_path_segments() {
        // Empty segment (e.g., trailing `:`) should not panic.
        let inputs = DetectInputs {
            path_override: Some(":::".to_string()),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_winning_canonical_is_first_match() {
        let td = TempDir::new().unwrap();
        let bin_a = td.path().join("a");
        let bin_b = td.path().join("b");
        make_exec(&bin_a.join("am"), b"#!/bin/sh\necho a");
        make_exec(&bin_b.join("am"), b"#!/bin/sh\necho b");
        let path_var = format!("{}:{}", bin_a.to_string_lossy(), bin_b.to_string_lossy());
        let findings = detect(&DetectInputs {
            path_override: Some(path_var),
        });
        let f = &findings[0];
        let canon_a = std::fs::canonicalize(bin_a.join("am")).unwrap();
        assert_eq!(f.winning_canonical, canon_a);
    }

    #[test]
    fn finding_severity_is_p1_detect_only() {
        let f = PathOrderShadowsAmFinding {
            hits: vec![],
            winning_canonical: PathBuf::from("/a/am"),
            distinct_canonical_count: 2,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P1");
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn manual_remediation_lists_each_hit_with_position() {
        let f = PathOrderShadowsAmFinding {
            hits: vec![
                AmHit {
                    path_index: 0,
                    probed: PathBuf::from("/home/op/.local/bin/am"),
                    canonical: PathBuf::from("/home/op/.local/bin/am"),
                },
                AmHit {
                    path_index: 3,
                    probed: PathBuf::from("/opt/py/bin/am"),
                    canonical: PathBuf::from("/opt/py/bin/am-real"),
                },
            ],
            winning_canonical: PathBuf::from("/home/op/.local/bin/am"),
            distinct_canonical_count: 2,
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("PATH[0]"));
        assert!(text.contains("PATH[3]"));
        assert!(text.contains("/home/op/.local/bin/am"));
        assert!(text.contains("/opt/py/bin/am-real"));
    }
}
