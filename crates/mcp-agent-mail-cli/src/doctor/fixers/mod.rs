//! Per-FM detector/fixer pairs for the world-class `am doctor` surface.
//!
//! Pass-8 introduces the FM (failure-mode) production pattern: each
//! detector is a pure function that scans system state and returns a
//! `Finding` list; each fixer takes a `Finding` plus a `MutateContext`
//! and routes its mutations through the chokepoint.
//!
//! Today the module hosts one concrete fixer
//! (`stale_archive_lock::detect` + `::fix`) as the reference pattern.
//! Pass-9+ adds the remaining priority FMs identified by Phase 3
//! synthesis (see `__doctor_workspace/analysis/dependency_graph.json`).
//!
//! Per AGENTS.md:
//! - No file deletion. Use `Op::Rename` to quarantine.
//! - asupersync only. Fixers are synchronous; the doctor runs out of
//!   band of the request hot path.
//! - `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

pub mod known_bad_git_no_override;
pub mod stale_archive_lock;
pub mod stale_head_or_ref_lock;
pub mod stale_listener_pid_hint;
pub mod world_readable_token_bak;
pub mod wrong_mcp_url_json;

use serde::Serialize;

/// `kill(pid, 0)` — POSIX liveness probe.
///
/// Shared helper for any fixer that needs to check whether a recorded
/// PID is still running. Returns `true` iff the process exists, including
/// when the caller lacks permission to signal it.
///
/// Caveat: `Pid::from_raw(0)` would refer to the calling process's
/// process group on POSIX, so PID 0 is rejected before probing. Tests
/// that want a guaranteed-dead PID should use `999_999_999` (above all
/// known `pid_max` values on Linux/macOS/BSD).
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }

    pid_probe_result_is_alive(nix::sys::signal::kill(Pid::from_raw(pid), None))
}

fn pid_probe_result_is_alive(result: Result<(), nix::errno::Errno>) -> bool {
    use nix::errno::Errno;

    matches!(result, Ok(()) | Err(Errno::EPERM))
}

/// One finding from a detector. Serializable for inclusion in
/// `report.json::findings[]`.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable ID, e.g. `"fm-archive-state-files-stale-archive-lock-from-dead-pid"`.
    pub id: &'static str,
    /// Severity tier: `"P0"` | `"P1"` | `"P2"` | `"P3"`.
    pub severity: &'static str,
    /// Subsystem from the 11-category Phase 1 taxonomy.
    pub subsystem: &'static str,
    /// One-line human-readable title.
    pub title: String,
    /// 0.0-1.0; ≥0.95 means the detector is certain.
    pub confidence: f32,
    /// Structured evidence: file:line, sql query, hash, etc.
    pub evidence: serde_json::Value,
    /// Suggested remediation command (for capabilities-routing).
    pub remediation: FindingRemediation,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingRemediation {
    pub command: String,
    pub explain_command: String,
    pub auto_fixable: bool,
    pub estimated_actions: usize,
}

/// Outcome of a fix attempt — what mutate() actions were taken.
#[derive(Debug, Default)]
pub struct FixOutcome {
    pub actions_taken: usize,
    pub actions_skipped: usize,
    pub quarantined_paths: Vec<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;

    #[test]
    fn pid_probe_result_treats_permission_denied_as_alive() {
        assert!(pid_probe_result_is_alive(Ok(())));
        assert!(pid_probe_result_is_alive(Err(Errno::EPERM)));
        assert!(!pid_probe_result_is_alive(Err(Errno::ESRCH)));
    }

    #[test]
    fn is_pid_alive_rejects_posix_special_or_unrepresentable_values() {
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(u32::MAX));
    }
}
