//! Unified process-owner model (br-bvq1x.9.4 / I4).
//!
//! ## Why this exists
//!
//! Even after DB-corruption diagnosis is fixed (Track A), agents kept
//! seeing `am` as "broken" when the *runtime* story was inconsistent:
//! a service manager that reports `active (running)` while no process is
//! actually serving the port; a stale PID hint pointing at a dead or
//! foreign process; a co-resident legacy **Python** Agent Mail server
//! racing the Rust server on `storage.sqlite3`; or a server running
//! entirely outside the supervisor. These were diagnosed piecemeal by
//! separate doctor FMs, each carrying only the slice of evidence it
//! happened to gather. No single surface answered the operator's actual
//! question: *who is supposed to be running, who is actually running,
//! who owns the port, which binary is it, and which DB does it touch?*
//!
//! [`ProcessOwnerModel`] is that single answer. It surfaces five
//! dimensions explicitly:
//!
//! 1. **expected-service** — what the service manager (systemd/launchd)
//!    believes it is running ([`ExpectedService`]).
//! 2. **actual-process** — the live process(es) holding the mailbox
//!    activity lock / DB file ([`ActualProcess`]).
//! 3. **port-owner** — who holds the configured `HTTP_HOST:HTTP_PORT`
//!    ([`PortOwnership`]).
//! 4. **binary-path** — the resolved executable of the owner(s) and of
//!    *this* `am` invocation.
//! 5. **DB-path** — the database file the model was resolved against.
//!
//! ## Purity contract (shared with B2)
//!
//! Everything in *this module* is pure: the types are plain data and the
//! `classify_*` functions are total functions over a model snapshot. They
//! perform **no** I/O, so the runtime FMs that consume them
//! ([`super::fixers::supervisor_respawn_loop`],
//! [`super::fixers::service_manager_divergence`]) stay observationally
//! pure and trivially testable with synthetic models.
//!
//! The impure half — reading systemd/launchd state, probing the port,
//! and enumerating `/proc` — lives in
//! `crate::gather_process_owner_model` (next to the other service
//! helpers in `lib.rs`), which is the single place that constructs a
//! model from the live host.

#![forbid(unsafe_code)]

use serde::Serialize;

/// Restart-count threshold at/above which a churning service is treated
/// as a respawn loop (see [`classify_supervisor_respawn`]).
pub const DEFAULT_RESPAWN_THRESHOLD: u32 = 3;

/// Which service manager (if any) is expected to own the `am` server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceManagerKind {
    Systemd,
    Launchd,
    /// No supervisor unit/plist installed for Agent Mail on this host.
    None,
}

/// Coarse service activity state, normalized across systemd `ActiveState`
/// and launchd. `NotApplicable` means there is no service manager to ask;
/// `Unknown` means there is one but its state could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceActiveState {
    Active,
    Activating,
    Deactivating,
    Reloading,
    Inactive,
    Failed,
    Unknown,
    NotApplicable,
}

impl ServiceActiveState {
    /// systemd `ActiveState=` → normalized state.
    #[must_use]
    pub fn from_systemd(value: &str) -> Self {
        match value.trim() {
            "active" => Self::Active,
            "activating" => Self::Activating,
            "deactivating" => Self::Deactivating,
            "reloading" => Self::Reloading,
            "inactive" => Self::Inactive,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    /// A "churning" state is one a respawn loop would currently sit in
    /// (as opposed to cleanly `Inactive` or healthily `Active`).
    #[must_use]
    pub fn is_churning(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Activating | Self::Deactivating | Self::Reloading
        )
    }
}

/// What the service manager *expects* to be running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectedService {
    pub manager: ServiceManagerKind,
    /// Whether a unit/plist is installed for Agent Mail.
    pub installed: bool,
    pub active_state: ServiceActiveState,
    /// systemd `SubState` (e.g. `running`, `auto-restart`, `failed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_state: Option<String>,
    /// systemd `Result` (e.g. `success`, `exit-code`, `signal`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// systemd `NRestarts` — cumulative restart count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n_restarts: Option<u32>,
    /// systemd `MainPID` (0 is normalized to `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_pid: Option<u32>,
    /// Bind host parsed from the unit/plist `ExecStart`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_host: Option<String>,
    /// Bind port parsed from the unit/plist `ExecStart`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_port: Option<u16>,
}

impl ExpectedService {
    /// A model for a host with no Agent Mail supervisor installed.
    #[must_use]
    pub fn none() -> Self {
        Self {
            manager: ServiceManagerKind::None,
            installed: false,
            active_state: ServiceActiveState::NotApplicable,
            sub_state: None,
            result: None,
            n_restarts: None,
            main_pid: None,
            configured_host: None,
            configured_port: None,
        }
    }
}

/// Classification of who holds the configured HTTP port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortOwnerClass {
    /// Nothing is listening on the port (bindable / no holders found).
    Free,
    /// At least one holder is a recognized Agent Mail (Rust) process.
    AgentMailSelf,
    /// The port is held, but by no recognized Agent Mail process.
    Foreign,
    /// Could not determine (e.g. listener enumeration unavailable here).
    Unknown,
}

/// Who owns the configured `HTTP_HOST:HTTP_PORT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortOwnership {
    pub host: String,
    pub port: u16,
    pub class: PortOwnerClass,
    /// PIDs found holding the port (best-effort; may be empty even when
    /// `reachable` is true on platforms without listener enumeration).
    pub holder_pids: Vec<u32>,
    /// A TCP connection to the port succeeded within the probe budget.
    pub reachable: bool,
}

/// A live process that currently holds the mailbox (activity lock or the
/// DB file). Derived from `inspect_mailbox_ownership`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActualProcess {
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The holder is a legacy **Python** Agent Mail server (coresident
    /// write risk — see `pid_is_agent_mail` in `db/src/pool.rs`).
    pub is_python_shadow: bool,
    /// The holder's executable was deleted out from under it (upgraded /
    /// removed while running).
    pub executable_deleted: bool,
    pub holds_lock: bool,
    pub holds_db_file: bool,
}

/// The single, unified process-owner model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessOwnerModel {
    pub expected_service: ExpectedService,
    pub actual_processes: Vec<ActualProcess>,
    pub port: PortOwnership,
    /// Resolved executable of *this* `am` invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_binary_path: Option<String>,
    pub db_path: String,
    pub storage_root: String,
}

impl ProcessOwnerModel {
    /// PIDs that hold the mailbox (lock or DB file).
    #[must_use]
    pub fn actual_owner_pids(&self) -> Vec<u32> {
        self.actual_processes.iter().map(|p| p.pid).collect()
    }

    /// True when at least one holder is a Python Agent Mail shadow.
    #[must_use]
    pub fn has_python_shadow(&self) -> bool {
        self.actual_processes.iter().any(|p| p.is_python_shadow)
    }

    /// Whether *something* recognizable as an Agent Mail server is live:
    /// an Agent Mail port owner, or a mailbox lock/DB holder.
    #[must_use]
    pub fn has_live_agent_mail(&self) -> bool {
        self.port.class == PortOwnerClass::AgentMailSelf || !self.actual_processes.is_empty()
    }
}

/// A respawn-loop verdict for the supervised service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupervisorRespawnVerdict {
    pub manager: ServiceManagerKind,
    pub n_restarts: u32,
    pub threshold: u32,
    pub active_state: ServiceActiveState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// Detect a supervisor respawn loop.
///
/// A *loop* is a service the supervisor keeps restarting that is **not**
/// currently healthy: restart count at/above `threshold` while the unit
/// sits in a churning state (`failed`/`activating`/`deactivating`/
/// `reloading`). A long-lived service that recovered (currently `active`)
/// or was cleanly stopped (`inactive`) is *not* flagged, even with a high
/// cumulative restart count, because `NRestarts` is cumulative-since-reset
/// rather than a rate.
#[must_use]
pub fn classify_supervisor_respawn(
    model: &ProcessOwnerModel,
    threshold: u32,
) -> Option<SupervisorRespawnVerdict> {
    let svc = &model.expected_service;
    // Only systemd exposes a reliable restart counter today.
    if svc.manager != ServiceManagerKind::Systemd {
        return None;
    }
    let n = svc.n_restarts?;
    if n < threshold {
        return None;
    }
    if !svc.active_state.is_churning() {
        return None;
    }
    Some(SupervisorRespawnVerdict {
        manager: svc.manager,
        n_restarts: n,
        threshold,
        active_state: svc.active_state,
        sub_state: svc.sub_state.clone(),
        result: svc.result.clone(),
    })
}

/// A specific way the service manager's view diverges from reality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceDivergenceKind {
    /// Supervisor reports `active`, but nothing is actually serving the
    /// mailbox (no port owner, no lock/DB holder).
    ManagerActiveNoServer,
    /// Supervisor's tracked `MainPID` is alive-tracked but is not among
    /// the real port / mailbox owners — it is managing the wrong process.
    MainPidNotOwner,
    /// A recognized Agent Mail server is running but the supervisor is not
    /// managing it (not installed, or it reports inactive/failed).
    UnmanagedServerRunning,
    /// The unit/plist `ExecStart` bind differs from the runtime config
    /// bind the model resolved against.
    ConfiguredBindMismatch,
    /// A live Python Agent Mail shadow holds the mailbox (coresident
    /// write race on `storage.sqlite3`).
    PythonShadowOwner,
}

impl ServiceDivergenceKind {
    /// Stable machine token used in evidence JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManagerActiveNoServer => "manager_active_no_server",
            Self::MainPidNotOwner => "main_pid_not_owner",
            Self::UnmanagedServerRunning => "unmanaged_server_running",
            Self::ConfiguredBindMismatch => "configured_bind_mismatch",
            Self::PythonShadowOwner => "python_shadow_owner",
        }
    }

    /// One-line operator-facing description of the divergence.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::ManagerActiveNoServer => {
                "service manager reports active but nothing is serving the mailbox port"
            }
            Self::MainPidNotOwner => {
                "service manager's tracked MainPID is not the process that owns the port/mailbox"
            }
            Self::UnmanagedServerRunning => {
                "an Agent Mail server is running outside the service manager (not installed or reported inactive/failed)"
            }
            Self::ConfiguredBindMismatch => {
                "the service unit's configured bind differs from the runtime config bind"
            }
            Self::PythonShadowOwner => {
                "a live Python Agent Mail server is holding the mailbox (coresident write race)"
            }
        }
    }
}

/// Compare a configured bind host against a runtime bind host. Wildcard
/// binds (`0.0.0.0` / `::` / empty) are treated as "matches anything" so
/// we do not flag a deliberately wide bind as a mismatch.
fn bind_host_matches(configured: &str, runtime: &str) -> bool {
    let c = configured.trim();
    let r = runtime.trim();
    let wildcard = |h: &str| matches!(h, "" | "0.0.0.0" | "::" | "*");
    wildcard(c) || wildcard(r) || c.eq_ignore_ascii_case(r)
}

/// Classify every way the supervisor's view diverges from reality.
///
/// Pure over the model snapshot; the ordering is stable (declaration
/// order of [`ServiceDivergenceKind`]) so callers/tests can rely on it.
#[must_use]
pub fn classify_service_manager_divergences(
    model: &ProcessOwnerModel,
) -> Vec<ServiceDivergenceKind> {
    let mut out = Vec::new();
    let svc = &model.expected_service;
    let port = &model.port;

    // 1. Active-but-no-server: supervisor says it is up, but no port owner
    //    and no mailbox lock/DB holder exists, and the port is not even
    //    reachable.
    if svc.active_state == ServiceActiveState::Active
        && !model.has_live_agent_mail()
        && !port.reachable
    {
        out.push(ServiceDivergenceKind::ManagerActiveNoServer);
    }

    // 2. MainPID-not-owner: the supervisor tracks a PID, there *is* a real
    //    owner to compare against, and the tracked PID is not among them.
    if let Some(main_pid) = svc.main_pid {
        let owners: Vec<u32> = port
            .holder_pids
            .iter()
            .copied()
            .chain(model.actual_owner_pids())
            .collect();
        if !owners.is_empty() && !owners.contains(&main_pid) {
            out.push(ServiceDivergenceKind::MainPidNotOwner);
        }
    }

    // 3. Unmanaged-server-running: a supervisor unit/plist IS installed,
    //    a recognized Agent Mail server is live, but the supervisor is not
    //    the thing managing it (reports inactive/failed). When no
    //    supervisor is installed at all, running `am serve-http` by hand is
    //    the normal mode — not a divergence — so this is gated on
    //    `installed`.
    let supervisor_managing = svc.installed && svc.active_state == ServiceActiveState::Active;
    if svc.installed && model.has_live_agent_mail() && !supervisor_managing {
        out.push(ServiceDivergenceKind::UnmanagedServerRunning);
    }

    // 4. Configured-bind-mismatch: the unit's bind differs from the
    //    runtime config bind (only when the unit declares both).
    if let (Some(cfg_host), Some(cfg_port)) = (svc.configured_host.as_deref(), svc.configured_port)
        && (!bind_host_matches(cfg_host, &port.host) || cfg_port != port.port)
    {
        out.push(ServiceDivergenceKind::ConfiguredBindMismatch);
    }

    // 5. Python-shadow-owner: surfaced at the model level so the unified
    //    surface lists it alongside the other divergences (the dedicated
    //    `stale_python_server_shadow` FM covers the PID-hint angle).
    if model.has_python_shadow() {
        out.push(ServiceDivergenceKind::PythonShadowOwner);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn agent_mail_owner(pid: u32) -> ActualProcess {
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

    fn python_owner(pid: u32) -> ActualProcess {
        ActualProcess {
            pid,
            binary_path: Some("/usr/bin/python3.11".into()),
            command: Some("python3 -m mcp_agent_mail.server".into()),
            is_python_shadow: true,
            executable_deleted: false,
            holds_lock: true,
            holds_db_file: false,
        }
    }

    #[test]
    fn active_state_systemd_mapping_and_churn() {
        assert_eq!(
            ServiceActiveState::from_systemd("active"),
            ServiceActiveState::Active
        );
        assert_eq!(
            ServiceActiveState::from_systemd("auto-restart-bogus"),
            ServiceActiveState::Unknown
        );
        assert!(ServiceActiveState::Failed.is_churning());
        assert!(ServiceActiveState::Activating.is_churning());
        assert!(!ServiceActiveState::Active.is_churning());
        assert!(!ServiceActiveState::Inactive.is_churning());
    }

    #[test]
    fn respawn_not_flagged_for_non_systemd() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Launchd,
            installed: true,
            active_state: ServiceActiveState::Failed,
            sub_state: None,
            result: Some("exit-code".into()),
            n_restarts: Some(99),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        };
        assert!(classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).is_none());
    }

    #[test]
    fn respawn_flagged_when_churning_over_threshold() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Activating,
            sub_state: Some("auto-restart".into()),
            result: Some("exit-code".into()),
            n_restarts: Some(7),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        };
        let v = classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).expect("loop");
        assert_eq!(v.n_restarts, 7);
        assert_eq!(v.threshold, DEFAULT_RESPAWN_THRESHOLD);
        assert_eq!(v.active_state, ServiceActiveState::Activating);
    }

    #[test]
    fn respawn_not_flagged_when_recovered_active() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active, // recovered
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(42), // high cumulative, but healthy now
            main_pid: Some(1000),
            configured_host: None,
            configured_port: None,
        };
        assert!(classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).is_none());
    }

    #[test]
    fn respawn_not_flagged_below_threshold() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Failed,
            sub_state: None,
            result: Some("signal".into()),
            n_restarts: Some(1),
            main_pid: None,
            configured_host: None,
            configured_port: None,
        };
        assert!(classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).is_none());
    }

    #[test]
    fn divergence_active_no_server() {
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
        // No owners, port not reachable.
        let d = classify_service_manager_divergences(&m);
        assert!(d.contains(&ServiceDivergenceKind::ManagerActiveNoServer));
    }

    #[test]
    fn divergence_active_with_real_server_is_clean() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: Some(4321),
            configured_host: Some("127.0.0.1".into()),
            configured_port: Some(8765),
        };
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::AgentMailSelf,
            holder_pids: vec![4321],
            reachable: true,
        };
        m.actual_processes = vec![agent_mail_owner(4321)];
        let d = classify_service_manager_divergences(&m);
        assert!(
            d.is_empty(),
            "healthy managed server must not diverge: {d:?}"
        );
    }

    #[test]
    fn divergence_main_pid_not_owner() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: Some(111),
            configured_host: Some("127.0.0.1".into()),
            configured_port: Some(8765),
        };
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::AgentMailSelf,
            holder_pids: vec![222], // different PID actually owns the port
            reachable: true,
        };
        m.actual_processes = vec![agent_mail_owner(222)];
        let d = classify_service_manager_divergences(&m);
        assert!(d.contains(&ServiceDivergenceKind::MainPidNotOwner));
    }

    #[test]
    fn divergence_unmanaged_server_running() {
        let mut m = base_model();
        // Supervisor IS installed but reports inactive, yet a server runs.
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Inactive,
            sub_state: Some("dead".into()),
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: None,
            configured_host: Some("127.0.0.1".into()),
            configured_port: Some(8765),
        };
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::AgentMailSelf,
            holder_pids: vec![900],
            reachable: true,
        };
        m.actual_processes = vec![agent_mail_owner(900)];
        let d = classify_service_manager_divergences(&m);
        assert!(d.contains(&ServiceDivergenceKind::UnmanagedServerRunning));
    }

    #[test]
    fn manual_server_without_supervisor_is_not_divergence() {
        // The common dev case: no systemd unit, `am serve-http` run by
        // hand. Must NOT be flagged as a divergence.
        let mut m = base_model();
        m.expected_service = ExpectedService::none();
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::AgentMailSelf,
            holder_pids: vec![900],
            reachable: true,
        };
        m.actual_processes = vec![agent_mail_owner(900)];
        let d = classify_service_manager_divergences(&m);
        assert!(
            d.is_empty(),
            "manual server without a supervisor must not diverge: {d:?}"
        );
    }

    #[test]
    fn divergence_configured_bind_mismatch() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Inactive,
            sub_state: None,
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: None,
            configured_host: Some("127.0.0.1".into()),
            configured_port: Some(9999), // unit binds 9999
        };
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765, // runtime config binds 8765
            class: PortOwnerClass::Free,
            holder_pids: Vec::new(),
            reachable: false,
        };
        let d = classify_service_manager_divergences(&m);
        assert!(d.contains(&ServiceDivergenceKind::ConfiguredBindMismatch));
    }

    #[test]
    fn divergence_wildcard_bind_does_not_mismatch() {
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Inactive,
            sub_state: None,
            result: Some("success".into()),
            n_restarts: Some(0),
            main_pid: None,
            configured_host: Some("0.0.0.0".into()), // wildcard
            configured_port: Some(8765),
        };
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::Free,
            holder_pids: Vec::new(),
            reachable: false,
        };
        let d = classify_service_manager_divergences(&m);
        assert!(!d.contains(&ServiceDivergenceKind::ConfiguredBindMismatch));
    }

    #[test]
    fn divergence_python_shadow_owner() {
        let mut m = base_model();
        m.actual_processes = vec![python_owner(700)];
        m.port = PortOwnership {
            host: "127.0.0.1".into(),
            port: 8765,
            class: PortOwnerClass::Foreign,
            holder_pids: vec![700],
            reachable: true,
        };
        let d = classify_service_manager_divergences(&m);
        assert!(d.contains(&ServiceDivergenceKind::PythonShadowOwner));
        assert!(m.has_python_shadow());
    }

    #[test]
    fn divergence_kind_tokens_stable() {
        assert_eq!(
            ServiceDivergenceKind::ManagerActiveNoServer.as_str(),
            "manager_active_no_server"
        );
        assert_eq!(
            ServiceDivergenceKind::PythonShadowOwner.as_str(),
            "python_shadow_owner"
        );
    }

    #[test]
    fn model_helpers() {
        let mut m = base_model();
        assert!(!m.has_live_agent_mail());
        m.actual_processes = vec![agent_mail_owner(5), python_owner(6)];
        assert_eq!(m.actual_owner_pids(), vec![5, 6]);
        assert!(m.has_python_shadow());
        assert!(m.has_live_agent_mail());
    }

    #[test]
    fn model_serializes_five_dimensions() {
        let mut m = base_model();
        m.actual_processes = vec![agent_mail_owner(5)];
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("expected_service").is_some());
        assert!(v.get("actual_processes").is_some());
        assert!(v.get("port").is_some());
        assert!(v.get("self_binary_path").is_some());
        assert!(v.get("db_path").is_some());
    }
}
