//! `fm-archive-state-files-missing-doctor-gitignore-entry` — P2.
//!
//! **Subsystem**: archive_state_files (Phase 1 archaeology — same
//! cluster as the other repo-state-files fixers).
//!
//! ## What's broken
//!
//! `am doctor --fix` writes per-run artifacts to `<repo>/.doctor/`.
//! If `.gitignore` doesn't exclude that directory, the operator's
//! next `git status` shows hundreds of `.doctor/runs/<id>/backups/...`
//! files as untracked, and a subsequent `git add -A` commits them.
//! The directory is design-for-non-tracking — committing it bloats
//! the repo and leaks per-machine bytes-witness hashes.
//!
//! The existing `runs::ensure_gitignore_entry` helper already
//! writes `.doctor/` to `.gitignore` as a side effect of every
//! `--fix` run, but it bypasses the `mutate()` chokepoint: no
//! verbatim backup, no `actions.jsonl` record, no hash recording,
//! no reversibility via `am doctor undo`. This FM lifts the same
//! check into a proper detector+fixer pair routed through
//! `Op::AppendFile`, so the gitignore line:
//!
//! 1. Has a backup of the pre-mutation file in
//!    `<run-dir>/backups/seq_<ns>/.gitignore`.
//! 2. Appears in `actions.jsonl` with before/after hashes.
//! 3. Is reversible via `am doctor undo <run-id>`.
//!
//! ## Detection (pure function)
//!
//! Given a path to a `.gitignore` candidate:
//! 1. If the file doesn't exist → emit a finding (needs creation).
//! 2. Otherwise read it line-by-line. If no line (trimmed) equals
//!    any of `REQUIRED_PATTERNS` → emit a finding listing the
//!    missing patterns.
//!
//! ## Fix (`Op::AppendFile` — new pattern)
//!
//! Build the missing-entries block (including a comment header)
//! and call `mutate(ctx, path, Op::AppendFile { content })`. The
//! chokepoint creates the file if absent (Op::AppendFile via
//! `OpenOptions::new().create(true).append(true)`), records the
//! before/after hashes, and writes the verbatim backup.
//!
//! Demonstrates the fifth canonical write-shape at FM level:
//! - Pass 8/9/10: `Op::Rename` (3 FMs)
//! - Pass 11: detect-only (1 FM)
//! - Pass 12: `Op::Chmod` (1 FM)
//! - Pass 13: `Op::WriteFile` (1 FM)
//! - **Pass 21: `Op::AppendFile`** ← this
//!
//! ## Reversibility
//!
//! `am doctor undo <run-id>` reads `actions.jsonl` in reverse:
//! - For a pre-existing `.gitignore`: restores the original bytes
//!   from the per-mutation seq backup.
//! - For a freshly-created `.gitignore`: the chokepoint's
//!   `before_hash` records the empty-file sentinel, and undo
//!   recreates that state (effectively removing the file, but
//!   per AGENTS.md RULE 1 the file is not deleted — undo restores
//!   the empty-content state instead).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-missing-doctor-gitignore-entry";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Canonical patterns the doctor expects in repo `.gitignore`. A line
/// matches a pattern if its trimmed form equals one of the variants.
///
/// `pub` so callers and tests reference this single source of truth
/// directly — same anti-drift pattern as pass-18's
/// `world_readable_token_bak::BACKUP_SUFFIX_HINTS`.
pub const REQUIRED_PATTERNS: &[GitignoreRequirement] = &[GitignoreRequirement {
    canonical: ".doctor/",
    accepted_variants: &[".doctor/", ".doctor"],
    comment: "am doctor per-run artifacts (world-class doctor surface)",
}];

/// One required-pattern entry. `canonical` is what we'd append; any
/// of `accepted_variants` (trimmed line equality) satisfies the check
/// — operators may already have `.doctor` without the trailing slash,
/// and we don't want to double-add.
#[derive(Debug, Clone, Copy)]
pub struct GitignoreRequirement {
    pub canonical: &'static str,
    pub accepted_variants: &'static [&'static str],
    pub comment: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingGitignoreEntryFinding {
    pub gitignore_path: PathBuf,
    /// The canonical patterns missing from the file. Order follows
    /// `REQUIRED_PATTERNS`.
    pub missing_canonical: Vec<&'static str>,
    /// Was the file present before the run? `false` means the fixer
    /// will create it.
    pub file_existed: bool,
}

impl MissingGitignoreEntryFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = if self.file_existed {
            format!(
                "{} is missing required entries {:?}",
                self.gitignore_path.display(),
                self.missing_canonical
            )
        } else {
            format!(
                "{} does not exist; doctor per-run artifacts would be committed",
                self.gitignore_path.display()
            )
        };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "gitignore_path": self.gitignore_path.to_string_lossy(),
                "missing_canonical": self.missing_canonical,
                "file_existed": self.file_existed,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE.
///
/// `gitignore_path` is typically `<repo>/.gitignore`. If `None` is
/// passed at the dispatcher level (no repo root resolvable), the
/// caller should pass an empty slice / skip this FM.
pub fn detect(gitignore_path: &std::path::Path) -> Vec<MissingGitignoreEntryFinding> {
    let (body, file_existed) = match fs::read_to_string(gitignore_path) {
        Ok(s) => (s, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(_) => return Vec::new(), // unreadable for some other reason — bail
    };

    let mut missing: Vec<&'static str> = Vec::new();
    for req in REQUIRED_PATTERNS {
        let satisfied = body
            .lines()
            .any(|line| req.accepted_variants.iter().any(|v| line.trim() == *v));
        if !satisfied {
            missing.push(req.canonical);
        }
    }

    if missing.is_empty() {
        return Vec::new();
    }

    vec![MissingGitignoreEntryFinding {
        gitignore_path: gitignore_path.to_path_buf(),
        missing_canonical: missing,
        file_existed,
    }]
}

/// Fixer. Routes through `mutate()` with `Op::AppendFile`.
///
/// Builds the append block from the finding's `missing_canonical`
/// list: one `# <comment>` line plus the canonical pattern per
/// requirement. Re-checks the file body before appending — if a
/// concurrent writer added the pattern between detect and fix, we
/// skip rather than duplicate.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &MissingGitignoreEntryFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    // Re-read the file. If a concurrent writer satisfied every
    // requirement, no-op (idempotent).
    let body = match fs::read_to_string(&finding.gitignore_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(crate::doctor::mutate::MutateError::Io(e)),
    };

    let mut payload = String::new();
    let mut count = 0usize;
    for req in REQUIRED_PATTERNS {
        if !finding.missing_canonical.contains(&req.canonical) {
            continue;
        }
        let already = body
            .lines()
            .any(|line| req.accepted_variants.iter().any(|v| line.trim() == *v));
        if already {
            continue;
        }
        // Leading newline to start a new block (only if the file is
        // non-empty AND doesn't already end with one). Op::AppendFile
        // appends raw bytes, so we shape the payload carefully.
        if payload.is_empty() && !body.is_empty() && !body.ends_with('\n') {
            payload.push('\n');
        }
        if !payload.is_empty() || body.is_empty() {
            // separate from prior content
        } else {
            payload.push('\n');
        }
        payload.push_str("# ");
        payload.push_str(req.comment);
        payload.push('\n');
        payload.push_str(req.canonical);
        payload.push('\n');
        count += 1;
    }

    if count == 0 {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }

    mutate(
        ctx,
        &finding.gitignore_path,
        Op::AppendFile {
            content: payload.into_bytes(),
        },
    )?;

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

    #[test]
    fn detector_returns_empty_when_pattern_present_with_slash() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\n.doctor/\nnode_modules/\n").unwrap();
        let findings = detect(&gi);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_returns_empty_when_pattern_present_without_slash() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\n.doctor\n").unwrap();
        let findings = detect(&gi);
        assert!(
            findings.is_empty(),
            "`.doctor` (no slash) must also satisfy the requirement"
        );
    }

    #[test]
    fn detector_flags_missing_pattern_when_file_exists() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\nnode_modules/\n").unwrap();
        let findings = detect(&gi);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].file_existed);
        assert!(findings[0].missing_canonical.contains(&".doctor/"));
    }

    #[test]
    fn detector_flags_when_file_absent() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        // Don't write the file.
        let findings = detect(&gi);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].file_existed);
        assert!(findings[0].missing_canonical.contains(&".doctor/"));
    }

    #[test]
    fn fixer_appends_required_pattern_via_mutate() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\n").unwrap();

        let findings = detect(&gi);
        assert_eq!(findings.len(), 1);
        let ctx = ctx_for(&td, "2026-05-12T00-00-00Z__gitignore");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let body = fs::read_to_string(&gi).unwrap();
        assert!(
            body.contains(".doctor/"),
            "fixer must append the canonical pattern (got: {body:?})"
        );
        assert!(
            body.starts_with("target/"),
            "pre-existing content must be preserved (got: {body:?})"
        );
    }

    #[test]
    fn fixer_creates_file_if_absent_via_mutate() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");

        let findings = detect(&gi);
        assert_eq!(findings.len(), 1);
        let ctx = ctx_for(&td, "2026-05-12T00-00-00Z__gitignore_create");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        assert!(gi.exists(), "fixer must have created the .gitignore");
        let body = fs::read_to_string(&gi).unwrap();
        assert!(body.contains(".doctor/"));
    }

    #[test]
    fn fixer_idempotent_when_pattern_added_between_detect_and_fix() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\n").unwrap();

        let findings = detect(&gi);
        assert_eq!(findings.len(), 1);

        // Simulate a concurrent writer adding the pattern.
        fs::write(&gi, "target/\n.doctor/\n").unwrap();

        let ctx = ctx_for(&td, "2026-05-12T00-00-00Z__gitignore_idemp");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        // Pattern is already satisfied, so the fixer skips.
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);

        // File body must be exactly what the concurrent writer left.
        let body = fs::read_to_string(&gi).unwrap();
        assert_eq!(body, "target/\n.doctor/\n");
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = MissingGitignoreEntryFinding {
            gitignore_path: PathBuf::from("/x/y/.gitignore"),
            missing_canonical: vec![".doctor/"],
            file_existed: true,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "archive_state_files");
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains(".doctor/"));
    }
}
