//! `fm-runtime-processes-stale-listener-pid-hint` — P1.
//!
//! **Subsystem**: runtime_processes (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! The mcp-agent-mail HTTP listener writes a PID hint file (e.g.,
//! `~/.mcp_agent_mail/listener.pid` or `<state-dir>/listener.pid`) on
//! startup so `am doctor check` and the TUI can find the running
//! listener cheaply. If the listener crashes / OOMs / gets SIGKILLed
//! before clearing the hint file, subsequent `am` invocations get
//! confused: they probe the hint's PID, find it dead, but the file
//! itself confounds liveness checks until cleaned up.
//!
//! Mitigations have landed over time (CHANGELOG.md fix 302 hardened
//! the `AlreadyExists`/`PermissionDenied` race; fix 556 hardened
//! symlink-TOCTOU on the hint file). But none of these are an
//! AUTO-FIX through the world-class chokepoint. Pass-9 adds that.
//!
//! ## Detection (pure function)
//!
//! Given a list of candidate hint paths (the project's known
//! locations), for each path:
//! 1. Must exist as a regular file (refuse symlinks — G4-style attack
//!    defense applies here too).
//! 2. Read the body for a PID (first whitespace-delimited token).
//! 3. If PID present and `kill(pid, 0)` succeeds → live listener → SKIP.
//! 4. If PID present and dead → STALE; emit finding.
//! 5. If no PID present → fall back to mtime: stale iff
//!    mtime > `stale_seconds` ago (default 600s / 10 min — longer than
//!    archive-lock since listener PID hints persist across restarts).
//!
//! ## Fix (routes through mutate)
//!
//! `mutate(ctx, hint_path, Op::Rename { to: quarantine })`. Quarantine
//! path is `<run-dir>/quarantine/listener-pid/<basename>.<ns>`. Per
//! AGENTS.md RULE 1, no deletion — quarantine only. Reversible via
//! `am doctor undo <run-id>`.
//!
//! ## Differences from stale_archive_lock
//!
//! - Different default threshold (10 min vs 5 min) — listener restarts
//!   are deliberate and may include brief downtime; archive locks
//!   suggest a stuck commit.
//! - Different subsystem (`runtime_processes` vs `archive_state_files`).
//! - The hint file path varies per platform/install; the detector
//!   takes the candidates as input rather than hard-coding.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-runtime-processes-stale-listener-pid-hint";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "runtime_processes";

/// Default mtime-based staleness threshold (10 minutes). Longer than
/// archive-lock (5 min) because listener restarts are deliberate and
/// may include brief planned downtime.
pub const DEFAULT_STALE_SECONDS: u64 = 600;

#[derive(Debug, Clone, Serialize)]
pub struct StaleListenerPidHintFinding {
    pub hint_path: PathBuf,
    pub recorded_pid: Option<u32>,
    pub age_seconds: u64,
    pub reason: StaleReason,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum StaleReason {
    /// Recorded PID is no longer running.
    DeadPid,
    /// No PID recorded; mtime exceeds the staleness threshold.
    Stale,
}

impl StaleListenerPidHintFinding {
    pub fn to_finding(&self) -> super::Finding {
        let reason_str = match self.reason {
            StaleReason::DeadPid => "dead PID",
            StaleReason::Stale => "stale mtime, no PID",
        };
        let title = format!(
            "stale listener PID hint at {} ({}, recorded_pid={:?}, age={}s)",
            self.hint_path.display(),
            reason_str,
            self.recorded_pid,
            self.age_seconds,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: match self.reason {
                StaleReason::DeadPid => 0.99,
                StaleReason::Stale => 0.80,
            },
            evidence: serde_json::json!({
                "hint_path": self.hint_path.to_string_lossy(),
                "recorded_pid": self.recorded_pid,
                "age_seconds": self.age_seconds,
                "reason": match self.reason {
                    StaleReason::DeadPid => "dead_pid",
                    StaleReason::Stale => "stale_mtime",
                },
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

/// Detector. PURE — no `mutate()` calls, no writes.
pub fn detect(
    candidate_hint_paths: &[PathBuf],
    stale_seconds: u64,
) -> Vec<StaleListenerPidHintFinding> {
    let mut out = Vec::new();
    let now = std::time::SystemTime::now();
    for hint_path in candidate_hint_paths {
        let meta = match fs::symlink_metadata(hint_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Symlink-attack defense: refuse to follow non-regular files.
        if !meta.file_type().is_file() {
            continue;
        }

        let recorded_pid = fs::read_to_string(hint_path)
            .ok()
            .and_then(|s| {
                s.lines()
                    .next()
                    .map(|first| first.split_whitespace().next().map(str::to_owned))
            })
            .flatten()
            .and_then(|tok| tok.parse::<u32>().ok());

        let age_seconds = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if let Some(pid) = recorded_pid {
            if super::is_pid_alive(pid) {
                // Live listener — leave the hint alone.
                continue;
            }
            out.push(StaleListenerPidHintFinding {
                hint_path: hint_path.clone(),
                recorded_pid: Some(pid),
                age_seconds,
                reason: StaleReason::DeadPid,
            });
        } else if age_seconds >= stale_seconds {
            // No PID + old → stale.
            out.push(StaleListenerPidHintFinding {
                hint_path: hint_path.clone(),
                recorded_pid: None,
                age_seconds,
                reason: StaleReason::Stale,
            });
        }
        // No PID + recent → conservative, skip.
    }
    out
}

/// Fixer. Routes the quarantine through `mutate()`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleListenerPidHintFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let basename = finding
        .hint_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "listener.pid".to_string());
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let quarantine = ctx
        .run_dir
        .join("quarantine")
        .join("listener-pid")
        .join(format!("{basename}.{now_ns}"));

    // Idempotent: if the hint vanished between detect and fix (the
    // listener restarted and cleaned up), no-op.
    if !finding.hint_path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }

    mutate(
        ctx,
        &finding.hint_path,
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

    #[test]
    fn detector_returns_empty_when_no_hint_files() {
        let td = TempDir::new().unwrap();
        let candidate = td.path().join("nonexistent.pid");
        let findings = detect(&[candidate], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_live_listener() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        let live_pid = std::process::id();
        fs::write(&hint, format!("{live_pid}\n")).unwrap();
        let findings = detect(&[hint], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty(), "live PID must NOT be reported");
    }

    #[test]
    fn detector_flags_dead_pid() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&hint), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, StaleReason::DeadPid);
        assert_eq!(findings[0].recorded_pid, Some(999_999_999));
        assert_eq!(findings[0].hint_path, hint);
    }

    #[test]
    fn detector_flags_stale_mtime_no_pid() {
        // Use a low stale threshold (0 secs) so even a just-touched
        // file qualifies as stale. This sidesteps the need to backdate
        // mtime without an extra crate dep.
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "").unwrap();
        let findings = detect(std::slice::from_ref(&hint), 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, StaleReason::Stale);
        assert_eq!(findings[0].recorded_pid, None);
    }

    #[test]
    fn detector_conservative_on_recent_no_pid() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "").unwrap();
        let findings = detect(&[hint], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty(), "recent empty hint must be left alone");
    }

    #[test]
    fn detector_refuses_symlink_hint() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("real_listener.pid");
        fs::write(&target, "999999999\n").unwrap();
        let hint = td.path().join("listener.pid");
        std::os::unix::fs::symlink(&target, &hint).unwrap();
        let findings = detect(&[hint], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty(), "symlink hint must be refused");
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = StaleListenerPidHintFinding {
            hint_path: PathBuf::from("/x/listener.pid"),
            recorded_pid: Some(999_999_999),
            age_seconds: 1200,
            reason: StaleReason::DeadPid,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P1");
        assert_eq!(g.subsystem, "runtime_processes");
        assert!(g.title.contains("listener PID hint"));
        assert!(g.title.contains("dead PID"));
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"reason\":\"dead_pid\""));
    }

    #[test]
    fn fixer_quarantines_stale_hint_via_mutate() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&hint), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);

        let run_id = "2026-05-10T08-30-00Z__stalepid";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.quarantined_paths.len(), 1);
        assert!(!hint.exists(), "hint must be removed");
        let q = &outcome.quarantined_paths[0];
        assert!(q.exists(), "quarantine must exist");
        assert_eq!(fs::read_to_string(q).unwrap(), "999999999\n");
    }

    #[test]
    fn fixer_idempotent_on_already_cleaned_hint() {
        let td = TempDir::new().unwrap();
        let finding = StaleListenerPidHintFinding {
            hint_path: td.path().join("nonexistent.pid"),
            recorded_pid: Some(999_999_999),
            age_seconds: 1200,
            reason: StaleReason::DeadPid,
        };
        let run_id = "2026-05-10T08-30-01Z__cleanup";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn fixer_then_undo_restores_hint() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&hint), DEFAULT_STALE_SECONDS);
        let run_id = "2026-05-10T08-30-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        drop(ctx);
        let summary = crate::doctor::undo::run_undo(td.path(), run_id, false, true).expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        assert!(hint.exists(), "undo must restore the hint file");
        assert_eq!(fs::read_to_string(&hint).unwrap(), "999999999\n");
    }
}
