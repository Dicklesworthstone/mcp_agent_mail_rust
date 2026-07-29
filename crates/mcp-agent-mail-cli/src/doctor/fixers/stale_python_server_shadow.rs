//! `fm-runtime-processes-stale-python-server-shadow` — P1.
//!
//! **Subsystem**: runtime_processes (Phase 1 archaeology — HANDOFF
//! P3-C #1 ranking, root of the dependency DAG).
//!
//! ## What's broken
//!
//! The Python `mcp-agent-mail` server (legacy implementation
//! the Rust port supersedes) and the Rust server share the
//! same `listener.pid` hint convention. If the Python server
//! was running on this host and its PID hint was left behind
//! when it shut down, the Rust server sees a "live PID" file
//! whose owner is a different process entirely. Worst case:
//! both servers run concurrently, fighting over the same
//! storage.sqlite3 — guaranteed data corruption.
//!
//! Pass-9 added `fm-runtime-processes-stale-listener-pid-hint`
//! which catches dead-PID and old-mtime cases. This FM catches
//! the "PID is alive but it's a Python interpreter, not the
//! Rust binary" case — distinct failure mode requiring different
//! evidence (the operator needs to know it's specifically a
//! Python server, not just any stale hint).
//!
//! ## Detection (pure function)
//!
//! For each `candidate_hint_paths` entry that exists:
//! 1. Read the recorded PID. If unparseable → skip.
//! 2. Check the PID is alive via `kill(pid, 0)`. If dead →
//!    skip (handled by the existing `stale_listener_pid_hint`
//!    FM at P1).
//! 3. Resolve `/proc/<pid>/exe` (Linux) or equivalent. If the
//!    target's basename starts with `python` (case-insensitive,
//!    `python3`, `python3.11`, `python3.12`, etc.), emit a
//!    finding. If `/proc` isn't readable (non-Linux, sandbox),
//!    no-op rather than false-positive.
//! 4. If the binary is the operator's Rust `mcp-agent-mail` →
//!    skip (healthy).
//!
//! ## Fix — detect-only
//!
//! Auto-fixing this FM is **dangerous**. Quarantining the
//! `.pid` file while the Python server is still running strips
//! the only lock that prevents a concurrent Rust + Python
//! server race — exactly the failure mode this FM is supposed
//! to prevent. The Rust server would happily start up after
//! the rename and proceed to write the same DB the Python
//! server is still writing.
//!
//! The correct workflow is:
//!
//! 1. `am doctor fix --only fm-...-stale-python-server-shadow --list`
//!    surfaces the finding (manual_remediation envelope).
//! 2. Operator manually `kill <pid>` on the Python interpreter.
//! 3. Next `am doctor` run sees a dead PID and the existing
//!    `stale_listener_pid_hint` FM (pass-9) safely quarantines
//!    the now-stale hint file via Op::Rename.
//!
//! So this FM is detect-only with auto_fixable=false; fix() is
//! a no-op for API uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-runtime-processes-stale-python-server-shadow";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "runtime_processes";

#[derive(Debug, Clone, Serialize)]
pub struct StalePythonServerShadowFinding {
    pub hint_path: PathBuf,
    pub recorded_pid: u32,
    /// `/proc/<pid>/exe` resolution. e.g. `/usr/bin/python3.11`.
    pub resolved_exe: PathBuf,
}

impl StalePythonServerShadowFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "listener.pid {} held by Python PID {} ({}) — shadow server risks data corruption",
            self.hint_path.display(),
            self.recorded_pid,
            self.resolved_exe.display()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "hint_path": self.hint_path.to_string_lossy(),
                "recorded_pid": self.recorded_pid,
                "resolved_exe": self.resolved_exe.to_string_lossy(),
                "risk": "concurrent Python + Rust servers race on storage.sqlite3 → data corruption",
                "manual_step": "Stop the Python interpreter first; then rerun am doctor so the dead listener.pid hint can be quarantined safely",
            }),
            remediation: FindingRemediation {
                // Detect-only: no auto-fix command. Operators must
                // manually `kill <pid>` the Python interpreter.
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE w.r.t. caller-supplied paths; reads /proc/<pid>/exe.
///
/// `candidate_hint_paths` is the same list `stale_listener_pid_hint`
/// uses — typically `<storage_root>/listener.pid` plus the operator's
/// XDG paths.
pub fn detect(candidate_hint_paths: &[PathBuf]) -> Vec<StalePythonServerShadowFinding> {
    let mut out = Vec::new();
    for hint_path in candidate_hint_paths {
        let meta = match fs::symlink_metadata(hint_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_file() {
            continue; // symlink-attack defense
        }
        let Some(pid) = read_recorded_pid(hint_path) else {
            continue;
        };
        if !super::is_pid_alive(pid) {
            continue; // dead-PID case is `stale_listener_pid_hint`
        }
        let Some(exe) = resolve_pid_exe(pid) else {
            continue;
        };
        if !is_python_interpreter(&exe) {
            continue; // healthy: the live PID is something else (Rust binary, etc.)
        }
        out.push(StalePythonServerShadowFinding {
            hint_path: hint_path.clone(),
            recorded_pid: pid,
            resolved_exe: exe,
        });
    }
    out
}

fn read_recorded_pid(hint_path: &std::path::Path) -> Option<u32> {
    let body = fs::read_to_string(hint_path).ok()?;
    body.lines()
        .next()?
        .split_whitespace()
        .next()?
        .parse::<u32>()
        .ok()
}

fn resolve_pid_exe(pid: u32) -> Option<PathBuf> {
    // Linux: /proc/<pid>/exe symlinks to the binary. Non-Linux:
    // return None (the detector will quietly no-op, matching
    // "we couldn't probe, so we won't false-positive").
    let exe_link = PathBuf::from(format!("/proc/{pid}/exe"));
    fs::read_link(&exe_link).ok()
}

fn is_python_interpreter(exe: &std::path::Path) -> bool {
    let Some(name) = exe.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();
    lower.starts_with("python")
        || lower.starts_with("pypy")
        // Some distros symlink python via /usr/bin/python -> ...
        || lower == "python"
}

/// Detect-only FM. `fix()` is a no-op for API uniformity —
/// auto-quarantining the lock file while the Python server is
/// still alive would cause the data race this FM is supposed
/// to prevent.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &StalePythonServerShadowFinding,
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

    #[test]
    fn is_python_interpreter_matches_common_names() {
        for name in [
            "python",
            "python3",
            "python3.11",
            "python3.12",
            "Python3",
            "/usr/bin/python3",
            "pypy",
            "pypy3.10",
        ] {
            assert!(
                is_python_interpreter(std::path::Path::new(name)),
                "should match: {name}"
            );
        }
        for name in [
            "/usr/bin/mcp-agent-mail",
            "am",
            "rustc",
            "/usr/bin/ruby",
            "/usr/bin/node",
        ] {
            assert!(
                !is_python_interpreter(std::path::Path::new(name)),
                "should NOT match: {name}"
            );
        }
    }

    #[test]
    fn detector_returns_empty_for_dead_pid() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        // PID 0 would be process group; use a guaranteed-dead pid above
        // all pid_max values.
        fs::write(&hint, "999999999\n").unwrap();
        let findings = detect(&[hint]);
        assert!(
            findings.is_empty(),
            "dead-PID case is stale_listener_pid_hint"
        );
    }

    #[test]
    fn detector_skips_when_pid_unparseable() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        fs::write(&hint, "not-a-pid\n").unwrap();
        let findings = detect(&[hint]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_when_hint_file_missing() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.pid")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_does_not_flag_current_rust_process() {
        // The test's own PID is a live process whose exe is `cargo test`
        // (definitely not python). Plant the hint with our own PID and
        // expect no finding.
        let td = TempDir::new().unwrap();
        let hint = td.path().join("listener.pid");
        let live_pid = std::process::id();
        fs::write(&hint, format!("{live_pid}\n")).unwrap();
        let findings = detect(&[hint]);
        assert!(
            findings.is_empty(),
            "current rust test process must not be flagged as python (got: {findings:?})"
        );
    }

    #[test]
    fn finding_is_p1_detect_only_with_python_evidence() {
        let f = StalePythonServerShadowFinding {
            hint_path: PathBuf::from("/x/listener.pid"),
            recorded_pid: 1234,
            resolved_exe: PathBuf::from("/usr/bin/python3.11"),
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P1");
        assert_eq!(g.subsystem, "runtime_processes");
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        assert!(g.remediation.command.contains("am doctor explain"));
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("python3.11"));
        assert!(s.contains("1234"));
        assert!(s.contains("Stop the Python interpreter first"));
    }
}
