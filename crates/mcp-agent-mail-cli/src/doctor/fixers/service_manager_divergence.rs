//! `fm-runtime-processes-service-manager-divergence` — P1.
//!
//! **Subsystem**: runtime_processes (br-bvq1x.9.4 / I4).
//!
//! ## What's broken
//!
//! The service manager's view of "the Agent Mail server" and the actual
//! runtime reality have drifted apart. Concrete forms (see
//! [`ServiceDivergenceKind`]):
//!
//! - **manager-active-no-server** — systemd reports `active (running)`
//!   but nothing is actually serving the port (the css/ts2 "service is
//!   `active` but port 8765 unreachable" symptom).
//! - **main-pid-not-owner** — the supervisor tracks one `MainPID` while a
//!   *different* process actually owns the port/mailbox.
//! - **unmanaged-server-running** — a server is live but the supervisor
//!   is not managing it (a hand-started `am serve-http` shadowing the
//!   unit, or a different binary path).
//! - **configured-bind-mismatch** — the unit binds a different host:port
//!   than the runtime config the model resolved against.
//! - **python-shadow-owner** — a legacy Python Agent Mail server is
//!   holding the mailbox (coresident write race).
//!
//! ## Detection (pure function)
//!
//! Pure over a [`ProcessOwnerModel`] snapshot via
//! [`crate::doctor::process_owner::classify_service_manager_divergences`].
//! Emits a single aggregated finding listing every divergence kind found,
//! plus the five process-owner dimensions (expected-service /
//! actual-process / port-owner / binary-path / DB-path).
//!
//! ## Fix — detect-only
//!
//! Reconciling supervisor state with reality is operator/supervisor
//! work, and `am doctor` never restarts or kills the supervised service
//! (the D4 "never kill `am`" contract). The finding carries the exact
//! `systemctl`/`launchctl` next steps. `fix()` is a no-op for API
//! uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::process_owner::{ProcessOwnerModel, ServiceDivergenceKind};
use serde::Serialize;

pub const FM_ID: &str = "fm-runtime-processes-service-manager-divergence";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "runtime_processes";

#[derive(Debug, Clone, Serialize)]
pub struct ServiceManagerDivergenceFinding {
    pub divergences: Vec<ServiceDivergenceKind>,
    /// Full process-owner snapshot (the five dimensions).
    pub model: ProcessOwnerModel,
}

impl ServiceManagerDivergenceFinding {
    pub fn to_finding(&self) -> super::Finding {
        let kinds: Vec<&'static str> = self.divergences.iter().map(|d| d.as_str()).collect();
        let descriptions: Vec<&'static str> =
            self.divergences.iter().map(|d| d.describe()).collect();
        let title = format!(
            "service-manager divergence ({}): {}",
            self.divergences.len(),
            descriptions.join("; ")
        );
        // Highest confidence for the unambiguous python-shadow and
        // active-no-server cases; bind-mismatch alone is a config skew.
        let confidence = if self
            .divergences
            .contains(&ServiceDivergenceKind::PythonShadowOwner)
        {
            0.95
        } else {
            0.85
        };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence,
            evidence: serde_json::json!({
                "divergences": kinds,
                // Explicit five-dimension surface.
                "expected_service": self.model.expected_service,
                "actual_processes": self.model.actual_processes,
                "port_owner": self.model.port,
                "binary_path": self.model.self_binary_path,
                "db_path": self.model.db_path,
                "manual_steps": [
                    "systemctl --user status agent-mail.service   # compare reported state to reality",
                    "am robot health --format json | jq .process_owner   # the unified runtime view",
                    "If a server is running outside the unit: stop it, then `systemctl --user restart agent-mail.service`",
                    "If a Python shadow is present: stop the Python interpreter first (it races storage.sqlite3)",
                ],
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

/// Detector. PURE over the supplied model snapshot. Emits at most one
/// aggregated finding (none when the supervisor view is consistent).
#[must_use]
pub fn detect(model: &ProcessOwnerModel) -> Vec<ServiceManagerDivergenceFinding> {
    let divergences = crate::doctor::process_owner::classify_service_manager_divergences(model);
    if divergences.is_empty() {
        Vec::new()
    } else {
        vec![ServiceManagerDivergenceFinding {
            divergences,
            model: model.clone(),
        }]
    }
}

/// Detect-only FM. `fix()` is a no-op — doctor never restarts or kills
/// the supervised service.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ServiceManagerDivergenceFinding,
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
        ActualProcess, ExpectedService, PortOwnerClass, PortOwnership, ServiceActiveState,
        ServiceManagerKind,
    };

    fn base_model() -> ProcessOwnerModel {
        ProcessOwnerModel {
            expected_service: ExpectedService::none(),
            actual_processes: Vec::new(),
            foreign_db_holders: Vec::new(),
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

    #[test]
    fn no_finding_when_consistent() {
        // No supervisor, nothing running → consistent (nothing to diverge).
        let m = base_model();
        assert!(detect(&m).is_empty());
    }

    #[test]
    fn aggregates_active_no_server() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        };
        let findings = detect(&m);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .divergences
                .contains(&ServiceDivergenceKind::ManagerActiveNoServer)
        );
    }

    #[test]
    fn python_shadow_raises_confidence() {
        let mut m = base_model();
        m.actual_processes = vec![ActualProcess {
            pid: 700,
            binary_path: Some("/usr/bin/python3.11".into()),
            command: Some("python3 -m mcp_agent_mail.server".into()),
            is_python_shadow: true,
            executable_deleted: false,
            holds_lock: true,
            holds_db_file: false,
        }];
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::Foreign,
            holder_pids: vec![700],
            reachable: true,
        };
        let f = detect(&m).remove(0).to_finding();
        assert!((f.confidence - 0.95).abs() < 1e-6);
        let v = serde_json::to_value(&f).unwrap();
        let kinds = v["evidence"]["divergences"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k == "python_shadow_owner"));
    }

    #[test]
    fn finding_surfaces_five_dimensions() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        };
        let f = detect(&m).remove(0).to_finding();
        assert_eq!(f.id, FM_ID);
        assert_eq!(f.severity, "P1");
        assert_eq!(f.subsystem, "runtime_processes");
        assert!(!f.remediation.auto_fixable);
        let v = serde_json::to_value(&f).unwrap();
        let ev = &v["evidence"];
        assert!(ev.get("expected_service").is_some());
        assert!(ev.get("actual_processes").is_some());
        assert!(ev.get("port_owner").is_some());
        assert!(ev.get("binary_path").is_some());
        assert!(ev.get("db_path").is_some());
    }
}
