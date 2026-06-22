//! `fm-runtime-processes-coresident-db-writer` — P0.
//!
//! **Subsystem**: runtime_processes (br-j3e9m; B6 audit
//! `docs/DOCTOR_FM_DISPOSITION.md` — historical FM
//! `python-server-coresident-write`).
//!
//! ## What's broken
//!
//! A live process **other than this Rust server** is holding the mailbox
//! `storage.sqlite3` open (or an Agent Mail advisory lock on it) and
//! racing it. The canonical instance is a legacy **Python**
//! `mcp_agent_mail` server left running on the same host: it does not
//! honor the Rust write-behind queue / commit coalescer / WAL discipline,
//! so two independent writers mutate the same B-tree pages and the DB
//! drifts into the `malformed disk image` corruption that wedged css/ts2
//! (see `integrity_page_malformed.rs:16`, `wal_shm_sidecar_drift.rs:32`).
//!
//! This is the **root cause**, detected *before* it corrupts. The sibling
//! FMs cover adjacent angles but not this one:
//!
//! - `integrity_page_malformed` (P0) detects the *symptom* (the DB is
//!   already corrupt) — too late to prevent it.
//! - `stale_python_server_shadow` (P1) detects a Python interpreter whose
//!   PID is recorded in a `listener.pid` *hint file* — it misses a live
//!   Python server that is actively holding the DB but is not the
//!   PID-hint owner (or when there is no hint file at all).
//! - `service_manager_divergence` (P1) surfaces `python_shadow_owner` as
//!   one facet of a *supervisor*-divergence aggregate, keyed on the
//!   process command line, not on whether it is concurrently open on the
//!   DB file. This FM is the dedicated, DB-concurrency-scoped, P0
//!   corruption-root-cause surface the disposition audit asked for.
//!
//! ## Detection (pure function)
//!
//! Pure over a [`ProcessOwnerModel`] snapshot (the same input
//! `service_manager_divergence` and `supervisor_respawn_loop` consume,
//! built once per run by `crate::gather_process_owner_model`). It flags
//! each [`ActualProcess`] that is **both**:
//!
//! 1. a foreign, uncoordinated writer (`is_python_shadow == true` — a
//!    Python `mcp_agent_mail` interpreter that bypasses the Rust write
//!    path), **and**
//! 2. concurrently engaged with the DB: it `holds_db_file` (an open fd on
//!    `storage.sqlite3`, confidence 1.0) or `holds_lock` (an Agent Mail
//!    advisory lock, confidence 0.9).
//!
//! A second *Rust* `am` instance (`is_python_shadow == false`) is **not**
//! flagged here: those honor the same WAL + lock protocol and are handled
//! by the ownership disposition / D4 supervised-owner guard
//! (split-brain / active-other-owner), a distinct failure mode with its
//! own surface. Flagging them here would double-report and dilute this
//! FM's specific "an *uncoordinated* writer is corrupting the DB" signal.
//!
//! ### Known scope limit (routed to a follow-up)
//!
//! `inspect_mailbox_ownership` (db crate) enumerates *every* PID holding
//! the DB file via a `/proc/*/fd` scan, but filters the result through
//! `pid_is_agent_mail()` before it reaches the model — so a truly foreign
//! writer that is **neither** the Rust binary **nor** a recognizable
//! Python `mcp_agent_mail` shadow (e.g. an ad-hoc `sqlite3` shell, a
//! migration script, a different language runtime) is dropped upstream and
//! is invisible to this pure detector. Surfacing those un-classified
//! foreign holders needs the unfiltered scan to be threaded into the
//! model; that broadening is tracked by `br-epoqj` rather than silently
//! implied here. The documented incident class (a co-resident Python
//! server) is fully covered.
//!
//! ## Fix — detect-only
//!
//! **Auto-fix is forbidden.** `am doctor` must never kill a foreign
//! process: the operator owns that decision, the PID may be load-bearing
//! for something else, and SIGKILL mid-write is itself a corruption
//! vector. The finding carries the manual triage path (identify → stop
//! the foreign writer → confirm DB integrity → reconstruct only if it
//! already corrupted). `fix()` is a no-op for API uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::process_owner::{ActualProcess, ProcessOwnerModel};
use serde::Serialize;

pub const FM_ID: &str = "fm-runtime-processes-coresident-db-writer";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "runtime_processes";

/// One live, foreign, co-resident writer racing the mailbox DB.
#[derive(Debug, Clone, Serialize)]
pub struct CoresidentDbWriterFinding {
    /// The mailbox database the writer is co-resident on
    /// (`ProcessOwnerModel::db_path`).
    pub db_path: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The holder is a legacy Python `mcp_agent_mail` server.
    pub is_python_shadow: bool,
    /// The holder has an open file descriptor on the DB file (the
    /// highest-confidence concurrency signal).
    pub holds_db_file: bool,
    /// The holder owns an Agent Mail advisory lock on the mailbox.
    pub holds_lock: bool,
}

impl CoresidentDbWriterFinding {
    #[must_use]
    pub fn from_process(db_path: &str, p: &ActualProcess) -> Self {
        Self {
            db_path: db_path.to_string(),
            pid: p.pid,
            binary_path: p.binary_path.clone(),
            command: p.command.clone(),
            is_python_shadow: p.is_python_shadow,
            holds_db_file: p.holds_db_file,
            holds_lock: p.holds_lock,
        }
    }

    /// Highest confidence when an open DB fd is directly observed; a
    /// lock-only holder is slightly lower (the lock proves Agent Mail
    /// protocol participation but not a concurrent page write right now).
    #[must_use]
    pub fn confidence(&self) -> f32 {
        if self.holds_db_file { 1.0 } else { 0.9 }
    }

    pub fn to_finding(&self) -> super::Finding {
        let how = if self.holds_db_file {
            "has storage.sqlite3 open"
        } else {
            "holds an Agent Mail advisory lock on the mailbox"
        };
        let title = format!(
            "live Python co-resident writer (PID {}{}) {how} — concurrent writes corrupt {}",
            self.pid,
            self.binary_path
                .as_deref()
                .map(|b| format!(", {b}"))
                .unwrap_or_default(),
            self.db_path,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: self.confidence(),
            evidence: serde_json::json!({
                "db_path": self.db_path,
                "pid": self.pid,
                "binary_path": self.binary_path,
                "command": self.command,
                "is_python_shadow": self.is_python_shadow,
                "holds_db_file": self.holds_db_file,
                "holds_lock": self.holds_lock,
                "risk": "an uncoordinated co-resident writer bypasses the Rust write-behind queue / commit coalescer / WAL discipline → two writers race the same B-tree pages → `database disk image is malformed` corruption",
                "manual_steps": [
                    format!("Confirm the holder: lsof -w {} (or: ls -l /proc/{}/exe; cat /proc/{}/cmdline | tr '\\0' ' ')", self.db_path, self.pid, self.pid),
                    format!("Stop the foreign writer gracefully (it is NOT this Rust server): kill {} — doctor will never do this for you", self.pid),
                    "Verify the DB is still intact: am doctor fix --only fm-db-state-files-integrity-page-malformed --list",
                    "Only if integrity_check already failed: am doctor reconstruct --yes (archive-first rebuild; reversible via undo)",
                ],
            }),
            remediation: FindingRemediation {
                // Detect-only: doctor never kills foreign processes.
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE over the supplied [`ProcessOwnerModel`] snapshot. Emits
/// one finding per live foreign co-resident writer (typically zero or
/// one). Stable ordering: declaration order of `model.actual_processes`.
#[must_use]
pub fn detect(model: &ProcessOwnerModel) -> Vec<CoresidentDbWriterFinding> {
    model
        .actual_processes
        .iter()
        .filter(|p| is_coresident_writer_risk(p))
        .map(|p| CoresidentDbWriterFinding::from_process(&model.db_path, p))
        .collect()
}

/// A holder is a co-resident-writer corruption risk when it is a foreign,
/// uncoordinated writer (a Python `mcp_agent_mail` shadow) that is also
/// concurrently engaged with the DB (open fd or advisory lock). Pulled out
/// as a named predicate so the scoping decision is testable in isolation.
#[must_use]
fn is_coresident_writer_risk(p: &ActualProcess) -> bool {
    p.is_python_shadow && (p.holds_db_file || p.holds_lock)
}

/// Detect-only FM. `fix()` is a no-op — doctor never kills a foreign
/// process (operator decision; SIGKILL mid-write is itself a corruption
/// vector).
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &CoresidentDbWriterFinding,
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
    use crate::doctor::process_owner::{
        ExpectedService, PortOwnerClass, PortOwnership, ProcessOwnerModel,
    };

    fn base_model() -> ProcessOwnerModel {
        ProcessOwnerModel {
            expected_service: ExpectedService::none(),
            actual_processes: Vec::new(),
            port: PortOwnership {
                host: "127.0.0.1".into(),
                port: 8765,
                class: PortOwnerClass::Free,
                holder_pids: Vec::new(),
                reachable: false,
            },
            self_binary_path: Some("/home/u/.local/bin/am".into()),
            db_path: "/srv/storage.sqlite3".into(),
            storage_root: "/srv".into(),
        }
    }

    fn python_writer(pid: u32, holds_db_file: bool, holds_lock: bool) -> ActualProcess {
        ActualProcess {
            pid,
            binary_path: Some("/usr/bin/python3.11".into()),
            command: Some("python3 -m mcp_agent_mail.server".into()),
            is_python_shadow: true,
            executable_deleted: false,
            holds_lock,
            holds_db_file,
        }
    }

    fn rust_owner(pid: u32) -> ActualProcess {
        ActualProcess {
            pid,
            binary_path: Some("/home/u/.local/bin/mcp-agent-mail".into()),
            command: Some("mcp-agent-mail serve-http".into()),
            is_python_shadow: false,
            executable_deleted: false,
            holds_lock: true,
            holds_db_file: true,
        }
    }

    #[test]
    fn no_finding_when_no_processes() {
        assert!(detect(&base_model()).is_empty());
    }

    #[test]
    fn flags_python_shadow_holding_db_file_at_full_confidence() {
        let mut m = base_model();
        m.actual_processes = vec![python_writer(700, true, false)];
        let findings = detect(&m);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.pid, 700);
        assert!(f.is_python_shadow);
        assert!(f.holds_db_file);
        assert!((f.confidence() - 1.0).abs() < 1e-6);
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "runtime_processes");
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        assert!((g.confidence - 1.0).abs() < 1e-6);
    }

    #[test]
    fn flags_python_shadow_holding_lock_only_at_lower_confidence() {
        let mut m = base_model();
        m.actual_processes = vec![python_writer(701, false, true)];
        let findings = detect(&m);
        assert_eq!(findings.len(), 1);
        assert!((findings[0].confidence() - 0.9).abs() < 1e-6);
        // Title reflects the lock-only path, not the open-fd path.
        assert!(findings[0].to_finding().title.contains("advisory lock"));
    }

    #[test]
    fn does_not_flag_python_shadow_that_holds_neither() {
        // A Python shadow that reached the model with neither the DB file
        // open nor a lock is not a *concurrent* writer risk for this FM.
        let mut m = base_model();
        m.actual_processes = vec![python_writer(702, false, false)];
        assert!(detect(&m).is_empty());
    }

    #[test]
    fn does_not_flag_second_rust_instance() {
        // A second Rust `am` holding the DB is split-brain / active-other-
        // owner (D4 / ownership disposition), not an uncoordinated foreign
        // writer. This FM must stay silent on it.
        let mut m = base_model();
        m.actual_processes = vec![rust_owner(900)];
        assert!(
            detect(&m).is_empty(),
            "a second Rust owner must not be flagged as a coresident foreign writer"
        );
    }

    #[test]
    fn flags_only_the_foreign_writer_in_a_mixed_set() {
        let mut m = base_model();
        m.actual_processes = vec![rust_owner(900), python_writer(700, true, true)];
        let findings = detect(&m);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].pid, 700);
    }

    #[test]
    fn finding_evidence_carries_db_path_pid_and_remediation() {
        let mut m = base_model();
        m.actual_processes = vec![python_writer(700, true, false)];
        let g = detect(&m).remove(0).to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("/srv/storage.sqlite3"));
        assert!(s.contains("700"));
        assert!(s.contains("am doctor reconstruct"));
        assert!(s.contains("fm-db-state-files-integrity-page-malformed"));
        // Never offers to kill the process automatically.
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn fix_is_a_noop() {
        let f = CoresidentDbWriterFinding {
            db_path: "/srv/storage.sqlite3".into(),
            pid: 700,
            binary_path: None,
            command: None,
            is_python_shadow: true,
            holds_db_file: true,
            holds_lock: false,
        };
        // fix() requires a MutateContext; assert the predicate + finding
        // shape stay detect-only without constructing a chokepoint ctx.
        assert!(!f.to_finding().remediation.auto_fixable);
        assert_eq!(f.to_finding().remediation.estimated_actions, 0);
    }
}
