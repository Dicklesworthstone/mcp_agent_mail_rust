//! `fm-runtime-processes-supervisor-respawn-loop` — P1.
//!
//! **Subsystem**: runtime_processes (br-bvq1x.9.4 / I4).
//!
//! ## What's broken
//!
//! The service manager (systemd `--user`) is stuck restarting the
//! `agent-mail` unit: the process starts, fails, and the supervisor
//! respawns it — over and over. To an agent the symptom is "`am` is
//! broken / flaky": the port flaps, health probes intermittently
//! succeed then fail, and a recurring crash (a degraded DB, a missing
//! binary, a bad bind) is hidden behind the supervisor's auto-restart.
//! This is the runtime sibling of the css incident where a systemd
//! service kept reviving an already-broken `am`.
//!
//! ## Detection (pure function)
//!
//! Pure over a [`ProcessOwnerModel`] snapshot via
//! [`crate::doctor::process_owner::classify_supervisor_respawn`]: a loop
//! is a systemd unit whose cumulative `NRestarts` is at/above the
//! threshold **and** which is currently churning (`failed` /
//! `activating` / `deactivating` / `reloading`). A unit that recovered
//! (currently `active`) or was cleanly stopped (`inactive`) is *not*
//! flagged, even with a high cumulative restart count.
//!
//! The finding carries the five process-owner dimensions
//! (expected-service / actual-process / port-owner / binary-path /
//! DB-path) so an operator sees the full runtime story, not just the
//! restart count.
//!
//! ## Fix — detect-only
//!
//! There is nothing safe to auto-mutate here. The remedy is operator/
//! supervisor action: read `journalctl --user -u agent-mail.service` for
//! the recurring crash cause, fix the root cause (often `am doctor
//! --json` for a degraded DB, or a stale/known-bad binary), then
//! `systemctl --user reset-failed agent-mail.service` and restart.
//! `am doctor` never restarts or kills the supervised service (the D4
//! "never kill `am`" contract). `fix()` is a no-op for API uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::process_owner::{
    DEFAULT_RESPAWN_THRESHOLD, ProcessOwnerModel, SupervisorRespawnVerdict,
};
use serde::Serialize;

pub const FM_ID: &str = "fm-runtime-processes-supervisor-respawn-loop";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "runtime_processes";

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorRespawnLoopFinding {
    pub verdict: SupervisorRespawnVerdict,
    /// Full process-owner snapshot (the five dimensions).
    pub model: ProcessOwnerModel,
}

impl SupervisorRespawnLoopFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "supervisor respawn loop: agent-mail restarted {} times and is currently {:?} — recurring crash hidden behind auto-restart",
            self.verdict.n_restarts, self.verdict.active_state
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.9,
            evidence: serde_json::json!({
                "respawn": self.verdict,
                // Explicit five-dimension surface.
                "expected_service": self.model.expected_service,
                "actual_processes": self.model.actual_processes,
                "port_owner": self.model.port,
                "binary_path": self.model.self_binary_path,
                "db_path": self.model.db_path,
                "manual_steps": [
                    "journalctl --user -u agent-mail.service -n 200 --no-pager   # find the recurring crash cause",
                    "am doctor --json   # the crash is often a degraded DB or stale/known-bad binary",
                    "systemctl --user reset-failed agent-mail.service && systemctl --user restart agent-mail.service",
                ],
                "risk": "auto-restart masks a recurring crash; the port flaps and health probes are intermittent",
            }),
            remediation: FindingRemediation {
                // Detect-only: doctor never restarts/kills the supervised
                // service (D4). Operator action required.
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE over the supplied model snapshot.
#[must_use]
pub fn detect(model: &ProcessOwnerModel, threshold: u32) -> Vec<SupervisorRespawnLoopFinding> {
    match crate::doctor::process_owner::classify_supervisor_respawn(model, threshold) {
        Some(verdict) => vec![SupervisorRespawnLoopFinding {
            verdict,
            model: model.clone(),
        }],
        None => Vec::new(),
    }
}

/// Detect with the canonical default threshold.
#[must_use]
pub fn detect_default(model: &ProcessOwnerModel) -> Vec<SupervisorRespawnLoopFinding> {
    detect(model, DEFAULT_RESPAWN_THRESHOLD)
}

/// Detect-only FM. `fix()` is a no-op — doctor never restarts or kills
/// the supervised service.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &SupervisorRespawnLoopFinding,
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
        ExpectedService, PortOwnerClass, PortOwnership, ServiceActiveState, ServiceManagerKind,
    };

    fn model_with_service(svc: ExpectedService) -> ProcessOwnerModel {
        ProcessOwnerModel {
            expected_service: svc,
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

    fn churning_systemd(n_restarts: u32) -> ExpectedService {
        ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Activating,
            sub_state: Some("auto-restart".into()),
            result: Some("exit-code".into()),
            n_restarts: Some(n_restarts),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        }
    }

    #[test]
    fn detects_respawn_loop() {
        let m = model_with_service(churning_systemd(5));
        let findings = detect_default(&m);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].verdict.n_restarts, 5);
    }

    #[test]
    fn no_finding_below_threshold() {
        let m = model_with_service(churning_systemd(1));
        assert!(detect_default(&m).is_empty());
    }

    #[test]
    fn no_finding_when_recovered() {
        let mut svc = churning_systemd(50);
        svc.active_state = ServiceActiveState::Active;
        svc.result = Some("success".into());
        let m = model_with_service(svc);
        assert!(detect_default(&m).is_empty());
    }

    #[test]
    fn no_finding_without_supervisor() {
        let m = model_with_service(ExpectedService::none());
        assert!(detect_default(&m).is_empty());
    }

    #[test]
    fn finding_is_p1_detect_only_with_five_dimensions() {
        let m = model_with_service(churning_systemd(4));
        let f = detect_default(&m).remove(0).to_finding();
        assert_eq!(f.id, FM_ID);
        assert_eq!(f.severity, "P1");
        assert_eq!(f.subsystem, "runtime_processes");
        assert!(!f.remediation.auto_fixable);
        assert_eq!(f.remediation.estimated_actions, 0);
        let v = serde_json::to_value(&f).unwrap();
        let ev = &v["evidence"];
        assert!(ev.get("expected_service").is_some());
        assert!(ev.get("actual_processes").is_some());
        assert!(ev.get("port_owner").is_some());
        assert!(ev.get("binary_path").is_some());
        assert!(ev.get("db_path").is_some());
        assert!(ev.get("respawn").is_some());
    }
}
