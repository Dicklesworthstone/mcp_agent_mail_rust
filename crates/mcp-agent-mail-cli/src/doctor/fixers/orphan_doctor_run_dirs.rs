//! `fm-doctor-state-files-orphan-run-dirs` — P2, auto-fix via
//! directory-tree `Op::Rename` (quarantine).
//!
//! **Subsystem**: doctor_state_files (internal doctor bookkeeping,
//! not user state).
//!
//! ## What's broken
//!
//! A doctor invocation that dies at scaffold time (killed session,
//! OOM, power loss) leaves `<repo>/.doctor/runs/<id>/` containing
//! only the scaffold skeleton — an empty `backups/` and an absent or
//! zero-byte `actions.jsonl`, with no `report.json` and no `latest`
//! symlink update. `am doctor health` resolves the latest run's
//! `report.json`; with only orphan scaffolds present it prints
//! "no report.json in latest run" and exits 1 **forever**, from every
//! session in that repo, with no doctor-native remediation:
//! `fm-doctor-state-files-dangling-latest-symlink` needs a symlink to
//! exist, and `am doctor reclaim` only consolidates STORAGE_ROOT
//! debris (br-p72wu; found by the N13 reliability gate when three
//! scaffold-time-death dirs from 2026-05-23 broke every repo-cwd
//! health assertion).
//!
//! ## Detection (pure function)
//!
//! For each direct, non-symlink subdirectory of `<repo>/.doctor/runs/`:
//! 1. `report.json` present → healthy completed run, skip.
//! 2. `actions.jsonl` present AND non-empty → the run recorded real
//!    mutations before dying. That is mid-fix crash evidence an
//!    operator should inspect (and `am doctor undo` may act on) —
//!    NOT no-op debris. Skip; health keeps failing loudly on it by
//!    design.
//! 3. `actions.jsonl` absent or zero-byte → provably no-op scaffold
//!    debris. Flag, subject to the age gate below.
//!
//! An age gate (default [`DEFAULT_MIN_AGE_SECONDS`]) plus an explicit
//! exclusion of the in-flight run's own directory keep the detector
//! from flagging a run that is currently mid-scaffold: every live
//! `--fix` invocation briefly looks exactly like case 3.
//!
//! ## Fix (directory-tree `Op::Rename` — quarantine)
//!
//! Each orphan run dir is MOVED (never deleted, per AGENTS.md RULE 1)
//! into `<run-dir>/quarantine/orphan-doctor-runs/<id>.<ns>` through
//! the `mutate()` chokepoint. Directory-tree renames are
//! hash-witnessed and `am doctor undo` restores them. The fixer
//! re-verifies the orphan predicate at fix time so a run that
//! completed between detect and fix is skipped, not quarantined.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-doctor-state-files-orphan-run-dirs";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "doctor_state_files";

/// Minimum age (by directory mtime) before an orphan scaffold is
/// eligible for quarantine. Protects concurrently-running doctor
/// invocations, whose fresh run dirs look exactly like orphans until
/// they write `actions.jsonl` content or `report.json`.
pub const DEFAULT_MIN_AGE_SECONDS: u64 = 3600;

/// How the orphan predicate was proven for `actions.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionsLedgerState {
    Absent,
    Empty,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanDoctorRunDirFinding {
    pub run_dir: PathBuf,
    pub run_name: String,
    pub actions_ledger: ActionsLedgerState,
    pub age_seconds: u64,
}

impl OrphanDoctorRunDirFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "orphan doctor run dir {} (no report.json, {} actions.jsonl) breaks `am doctor health`",
            self.run_dir.display(),
            match self.actions_ledger {
                ActionsLedgerState::Absent => "no",
                ActionsLedgerState::Empty => "empty",
            },
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "run_dir": self.run_dir.to_string_lossy(),
                "run_name": self.run_name,
                "actions_ledger": self.actions_ledger,
                "age_seconds": self.age_seconds,
                "remediation_strategy": "quarantine the whole run dir via directory-tree Op::Rename; runs with recorded actions are crash evidence and are never touched",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Classify one run dir against the orphan predicate. PURE.
///
/// Returns `Some(state)` when the dir has no `report.json` and a
/// provably no-op actions ledger (absent or zero-byte regular file).
fn orphan_actions_state(run_dir: &Path) -> Option<ActionsLedgerState> {
    if fs::symlink_metadata(run_dir.join("report.json")).is_ok() {
        return None; // completed run (or at least a report exists)
    }
    match fs::symlink_metadata(run_dir.join("actions.jsonl")) {
        Err(_) => Some(ActionsLedgerState::Absent),
        Ok(meta) if meta.file_type().is_file() && meta.len() == 0 => {
            Some(ActionsLedgerState::Empty)
        }
        // Non-empty ledger = mid-fix crash evidence; non-regular
        // ledger = unusual state. Both stay untouched and loud.
        Ok(_) => None,
    }
}

/// Detector. PURE.
///
/// Scans the direct, non-symlink subdirectories of `runs_dir`
/// (typically `<repo>/.doctor/runs`). `exclude_run_dir` is the
/// in-flight invocation's own run dir, which always looks like an
/// orphan while it is mid-scaffold. `min_age_seconds` gates on the
/// run dir's mtime (see [`DEFAULT_MIN_AGE_SECONDS`]).
pub fn detect(
    runs_dir: &Path,
    min_age_seconds: u64,
    exclude_run_dir: Option<&Path>,
) -> Vec<OrphanDoctorRunDirFinding> {
    let Ok(entries) = fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now();
    let mut findings = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue; // files and symlinks are out of scope
        }
        if exclude_run_dir.is_some_and(|excluded| paths_refer_to_same_dir(&path, excluded)) {
            continue;
        }
        let Some(actions_ledger) = orphan_actions_state(&path) else {
            continue;
        };
        let age_seconds = meta
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map_or(0, |age| age.as_secs());
        if age_seconds < min_age_seconds {
            continue;
        }
        let run_name = entry.file_name().to_string_lossy().into_owned();
        findings.push(OrphanDoctorRunDirFinding {
            run_dir: path,
            run_name,
            actions_ledger,
            age_seconds,
        });
    }
    findings.sort_by(|a, b| a.run_name.cmp(&b.run_name));
    findings
}

/// Compare two directory paths for identity, tolerating one side
/// being non-canonical. Falls back to literal equality when either
/// side cannot be canonicalized.
fn paths_refer_to_same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Fixer. Routes through `mutate()` with a directory-tree
/// `Op::Rename` into the current run's quarantine.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &OrphanDoctorRunDirFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    // Re-verify the orphan predicate at fix time: the dir may have
    // vanished, or a slow run may have completed (report.json) or
    // recorded actions since detection. Quarantining either would be
    // wrong, so those skip.
    match fs::symlink_metadata(&finding.run_dir) {
        Err(_) => {
            return Ok(FixOutcome {
                actions_taken: 0,
                actions_skipped: 1,
                quarantined_paths: Vec::new(),
            });
        }
        Ok(meta) if !meta.file_type().is_dir() => {
            return Ok(FixOutcome {
                actions_taken: 0,
                actions_skipped: 1,
                quarantined_paths: Vec::new(),
            });
        }
        Ok(_) => {}
    }
    if orphan_actions_state(&finding.run_dir).is_none() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let quarantine = ctx
        .run_dir
        .join("quarantine")
        .join("orphan-doctor-runs")
        .join(format!("{}.{now_ns}", finding.run_name));

    mutate(
        ctx,
        &finding.run_dir,
        Op::Rename {
            to: quarantine.clone(),
        },
    )?;

    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: vec![quarantine],
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

    fn make_orphan(runs_dir: &Path, name: &str, ledger: Option<&[u8]>) -> PathBuf {
        let dir = runs_dir.join(name);
        fs::create_dir_all(dir.join("backups")).unwrap();
        if let Some(bytes) = ledger {
            fs::write(dir.join("actions.jsonl"), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn detector_returns_empty_when_runs_dir_missing() {
        let td = TempDir::new().unwrap();
        let findings = detect(&td.path().join(".doctor").join("runs"), 0, None);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_completed_runs_and_crash_evidence() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        // Completed run: report.json present.
        let completed = make_orphan(&runs, "2026-05-23T00-00-00Z__done", None);
        fs::write(completed.join("report.json"), b"{}").unwrap();
        // Crash evidence: no report, but the ledger recorded actions.
        make_orphan(
            &runs,
            "2026-05-23T01-00-00Z__crashed",
            Some(b"{\"op\":\"Rename\"}\n"),
        );

        let findings = detect(&runs, 0, None);
        assert!(
            findings.is_empty(),
            "completed runs and mid-fix crash evidence must never be flagged: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_scaffold_death_orphans() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        make_orphan(&runs, "2026-05-23T02-00-00Z__only_backups", None);
        make_orphan(&runs, "2026-05-23T03-00-00Z__empty_ledger", Some(b""));

        let findings = detect(&runs, 0, None);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].actions_ledger, ActionsLedgerState::Absent);
        assert_eq!(findings[1].actions_ledger, ActionsLedgerState::Empty);
    }

    #[test]
    fn detector_age_gate_protects_fresh_scaffolds() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        make_orphan(&runs, "2026-05-23T04-00-00Z__fresh", None);

        let findings = detect(&runs, DEFAULT_MIN_AGE_SECONDS, None);
        assert!(
            findings.is_empty(),
            "a just-created scaffold must not be flagged"
        );
    }

    #[test]
    fn detector_excludes_the_in_flight_run_dir() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        let own = make_orphan(&runs, "2026-05-23T05-00-00Z__in_flight", Some(b""));

        let findings = detect(&runs, 0, Some(&own));
        assert!(
            findings.is_empty(),
            "the current invocation's own run dir must be excluded"
        );
    }

    #[test]
    fn detector_ignores_symlinked_directories() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        fs::create_dir_all(&runs).unwrap();
        let outside = td.path().join("outside-run");
        fs::create_dir_all(outside.join("backups")).unwrap();
        std::os::unix::fs::symlink(&outside, runs.join("linked-run")).unwrap();

        let findings = detect(&runs, 0, None);
        assert!(
            findings.is_empty(),
            "symlinked entries under runs/ must not be flagged (the link target may be live state)"
        );
    }

    #[test]
    fn fixer_quarantines_the_whole_orphan_tree() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        let orphan = make_orphan(&runs, "2026-05-23T06-00-00Z__orphan", Some(b""));

        let findings = detect(&runs, 0, None);
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-23T07-00-00Z__orphan_fix");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.quarantined_paths.len(), 1);
        assert!(
            !orphan.exists(),
            "orphan dir must be gone from the live runs/ path"
        );
        let quarantined = &outcome.quarantined_paths[0];
        assert!(
            quarantined.starts_with(ctx.run_dir.join("quarantine").join("orphan-doctor-runs")),
            "quarantine destination must live under the current run dir: {}",
            quarantined.display()
        );
        assert!(
            quarantined.join("backups").is_dir(),
            "the whole tree (including backups/) must survive the move"
        );
    }

    #[test]
    fn fixer_skips_when_dir_vanished_or_completed() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join(".doctor").join("runs");
        let orphan = make_orphan(&runs, "2026-05-23T08-00-00Z__races", Some(b""));
        let findings = detect(&runs, 0, None);
        assert_eq!(findings.len(), 1);

        // The run completed between detect and fix: report.json appeared.
        fs::write(orphan.join("report.json"), b"{}").unwrap();
        let ctx = ctx_for(&td, "2026-05-23T09-00-00Z__races_fix");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(orphan.exists(), "a completed run must never be quarantined");

        // And once the dir is genuinely gone the fixer is idempotent.
        fs::remove_dir_all(&orphan).unwrap();
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = OrphanDoctorRunDirFinding {
            run_dir: PathBuf::from("/x/.doctor/runs/only_abc"),
            run_name: "only_abc".to_string(),
            actions_ledger: ActionsLedgerState::Absent,
            age_seconds: 12_345,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "doctor_state_files");
        assert!(g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("only_abc"));
        assert!(s.contains("absent"));
    }
}
