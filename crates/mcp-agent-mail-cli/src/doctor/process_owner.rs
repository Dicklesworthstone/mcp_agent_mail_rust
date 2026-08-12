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
    /// Age of the supervised main process in seconds, derived from systemd's
    /// `ExecMainStartTimestampMonotonic` against the host uptime. A young
    /// main process on a unit with a high cumulative restart count is a
    /// crash loop sampled between crashes (br-uc6sb, ts1: NRestarts=8670
    /// while `active (running)`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_pid_age_seconds: Option<u64>,
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
            main_pid_age_seconds: None,
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

/// A truly foreign process holding the mailbox DB file open — neither this Rust
/// binary nor a recognizable Python `mcp_agent_mail` shadow (br-epoqj).
///
/// `inspect_mailbox_ownership` filters those out before they reach
/// [`ActualProcess`], so they are carried separately here and surfaced as a
/// lower-confidence, detect-only finding by
/// [`super::fixers::coresident_db_writer`]. Mirrors
/// `mcp_agent_mail_db::pool::ForeignDbFileHolder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForeignDbHolder {
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The holder's executable was deleted out from under it.
    pub executable_deleted: bool,
}

/// The single, unified process-owner model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessOwnerModel {
    pub expected_service: ExpectedService,
    pub actual_processes: Vec<ActualProcess>,
    /// Unfiltered, un-classified foreign holders of the mailbox DB file
    /// (neither this Rust binary nor a Python shadow). Detect-only forensics
    /// (br-epoqj); empty on hosts without `/proc`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_db_holders: Vec<ForeignDbHolder>,
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
    // A loop is usually SAMPLED between crashes: the unit reads `active`
    // with a freshly-respawned main process (ts1: NRestarts=8670 while
    // `active (running)` — invisible to the churning-only rule, br-uc6sb).
    // `active` + threshold-crossing count + a young supervised main process
    // is a loop caught mid-respawn; `active` with an OLD main process is a
    // long-recovered service and stays unflagged (NRestarts is cumulative).
    let active_young_main = svc.active_state == ServiceActiveState::Active
        && svc
            .main_pid_age_seconds
            .is_some_and(|age| age <= RESPAWN_YOUNG_MAIN_PROCESS_SECS);
    if !svc.active_state.is_churning() && !active_young_main {
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

/// Maximum age of the supervised main process for the `active`-state loop
/// rule in [`classify_supervisor_respawn`]: with the restart threshold also
/// crossed, a main process younger than this means the "healthy-looking"
/// unit was respawned moments ago.
pub const RESPAWN_YOUNG_MAIN_PROCESS_SECS: u64 = 300;

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

// ─── br-z41ij: reclaimable-owner classification for supervised takeover ───────
//
// ## The corruption-epoch deadlock this breaks
//
// When a mailbox owner PID wedges — most acutely a live process running a
// *deleted* executable that keeps holding the exclusive activity lock — the
// existing supervised-owner guard (br-bvq1x.4.4 / D4) correctly refuses every
// mutating doctor verb (`repair`/`reconstruct` exit 3). That refusal is the
// right default: repairing under a live writer risks interleaving. But when the
// owner is genuinely *dead-or-wedged*, "refuse forever" is the worst possible
// multi-agent outcome: fleet evidence (session 575f27e7, 2026-06-24..29) shows
// a whole swarm on git-ledger fallback for ~5 days while a wedged deleted-binary
// PID held the lock ~4.3 days, every agent correctly deferring, nobody able to
// heal. `--allow-live-owner` is too blunt to offer an agent (it overrides *every*
// class, including a healthy live server).
//
// This module supplies the pure, conservative discriminator that lets a
// *supervised takeover* (`am doctor repair --take-ownership`) act only when the
// owner is provably reclaimable, so a healthy in-place-upgraded server (also a
// deleted-executable owner!) is never disturbed.
//
// ## Purity contract
//
// [`ReclaimableOwnerEvidence::verdict`] and [`owner_set_is_reclaimable`] are
// total functions over plain-data evidence — no `/proc` reads, no port probes.
// The impure half (reading `/proc/<pid>/stat` for the zombie state, the process
// age, and probing the mailbox port for responsiveness) is gathered by the CLI
// (`crates/mcp-agent-mail-cli/src/lib.rs`) and fed in as evidence, so the
// classification stays trivially testable with synthetic inputs.

/// Default idle threshold (seconds) a *non-zombie* deleted-executable owner must
/// exceed before its stale mailbox locks are considered reclaimable without a
/// supervised drain (br-z41ij).
///
/// A live binary upgrade re-opens the database within seconds; a process that
/// has held the lock for this long while the mailbox is unresponsive is wedged,
/// not mid-upgrade. 10 minutes is comfortably longer than any legitimate
/// restart/upgrade window and far shorter than the multi-day epochs the fleet
/// actually suffered.
pub const RECLAIMABLE_OWNER_MIN_IDLE_SECS: u64 = 10 * 60;

/// Why a lock-holding owner was (or was not) judged reclaimable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimVerdict {
    /// The holder is a zombie/defunct process (`/proc/<pid>/stat` state `Z`):
    /// the kernel has already closed every fd and released every lock it held,
    /// so any lock file naming it is a stale artifact.
    ZombieHolder,
    /// The holder runs a deleted executable, the mailbox is unresponsive, and it
    /// has been idle past the threshold — a wedged owner, not a live upgrade.
    WedgedDeletedExecutable,
    /// Not reclaimable: the evidence is consistent with a live, working owner
    /// (or is too weak to rule one out). The safe default.
    NotReclaimable,
}

impl ReclaimVerdict {
    /// Stable machine token used in evidence/report JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZombieHolder => "zombie_holder",
            Self::WedgedDeletedExecutable => "wedged_deleted_executable",
            Self::NotReclaimable => "not_reclaimable",
        }
    }

    /// Whether this verdict permits a supervised takeover of the owner's locks.
    #[must_use]
    pub fn is_reclaimable(self) -> bool {
        !matches!(self, Self::NotReclaimable)
    }
}

/// Per-process evidence that a mailbox lock holder is dead-or-wedged and its
/// activity locks can be safely quarantined so a repair owner can take over
/// (br-z41ij).
///
/// Every field is plain data gathered by the CLI; [`Self::verdict`] is pure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReclaimableOwnerEvidence {
    pub pid: u32,
    /// `/proc/<pid>/stat` process state is `Z` (zombie/defunct).
    pub zombie: bool,
    /// The owner's executable was deleted/replaced out from under it
    /// (`/proc/<pid>/exe` resolves to a `… (deleted)` path).
    pub executable_deleted: bool,
    /// A fresh liveness probe found the mailbox still serving (its configured
    /// HTTP port accepted a connection). A responsive owner is a *working*
    /// owner — never reclaimable — which is what protects a healthy server that
    /// merely had its binary replaced in place.
    pub mailbox_responsive: bool,
    /// Age of the holding process in seconds, when resolvable from `/proc`.
    /// Unknown age is treated as "too fresh to judge" (not reclaimable).
    pub age_seconds: Option<u64>,
    /// Idle threshold applied to the deleted-executable branch.
    pub min_idle_seconds: u64,
}

impl ReclaimableOwnerEvidence {
    /// Classify a single holder. Conservative by construction: a zombie is
    /// reclaimable outright (it holds nothing); every other holder must clear
    /// *all* of deleted-executable, unresponsive, and idle-past-threshold. Any
    /// missing or ambiguous signal yields [`ReclaimVerdict::NotReclaimable`].
    #[must_use]
    pub fn verdict(&self) -> ReclaimVerdict {
        if self.zombie {
            return ReclaimVerdict::ZombieHolder;
        }
        let idle_long_enough = self
            .age_seconds
            .is_some_and(|age| age >= self.min_idle_seconds);
        if self.executable_deleted && !self.mailbox_responsive && idle_long_enough {
            return ReclaimVerdict::WedgedDeletedExecutable;
        }
        ReclaimVerdict::NotReclaimable
    }

    /// Convenience: whether this holder alone is reclaimable.
    #[must_use]
    pub fn is_reclaimable(&self) -> bool {
        self.verdict().is_reclaimable()
    }
}

/// Aggregate reclaimability over the full candidate-owner set.
///
/// The set is reclaimable only when it is **non-empty** and **every** candidate
/// is individually reclaimable. A single live, non-reclaimable holder makes a
/// supervised takeover unsafe — quarantining the shared lock would be stealing
/// it from a process that may still be writing — so we refuse the whole set.
#[must_use]
pub fn owner_set_is_reclaimable(evidence: &[ReclaimableOwnerEvidence]) -> bool {
    !evidence.is_empty()
        && evidence
            .iter()
            .all(ReclaimableOwnerEvidence::is_reclaimable)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
            configured_host: None,
            configured_port: None,
        };
        let v = classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).expect("loop");
        assert_eq!(v.n_restarts, 7);
        assert_eq!(v.threshold, DEFAULT_RESPAWN_THRESHOLD);
        assert_eq!(v.active_state, ServiceActiveState::Activating);
    }

    #[test]
    fn respawn_flagged_when_active_with_young_main_over_threshold() {
        // ts1 (br-uc6sb): NRestarts=8670 sampled while `active (running)` —
        // a loop is usually observed BETWEEN crashes, with a main process
        // respawned moments ago.
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(8670),
            main_pid: Some(2860),
            main_pid_age_seconds: Some(30),
            configured_host: None,
            configured_port: None,
        };
        let v = classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD)
            .expect("active-but-freshly-respawned loop must be flagged");
        assert_eq!(v.n_restarts, 8670);
        assert_eq!(v.active_state, ServiceActiveState::Active);
    }

    #[test]
    fn respawn_not_flagged_when_active_young_main_below_threshold() {
        // A freshly (re)started healthy service with a low restart count is
        // a normal restart, not a loop.
        let mut m = base_model();
        m.expected_service = ExpectedService {
            manager: ServiceManagerKind::Systemd,
            installed: true,
            active_state: ServiceActiveState::Active,
            sub_state: Some("running".into()),
            result: Some("success".into()),
            n_restarts: Some(1),
            main_pid: Some(2860),
            main_pid_age_seconds: Some(30),
            configured_host: None,
            configured_port: None,
        };
        assert!(classify_supervisor_respawn(&m, DEFAULT_RESPAWN_THRESHOLD).is_none());
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
            main_pid_age_seconds: Some(86_400), // long-lived main = recovered
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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
            main_pid_age_seconds: None,
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

    // ─── br-z41ij: reclaimable-owner classification ─────────────────────────

    fn wedged_evidence(pid: u32) -> ReclaimableOwnerEvidence {
        // A live (non-zombie) owner running a deleted binary, mailbox not
        // serving, idle well past the threshold — the fleet's actual wedge.
        ReclaimableOwnerEvidence {
            pid,
            zombie: false,
            executable_deleted: true,
            mailbox_responsive: false,
            age_seconds: Some(RECLAIMABLE_OWNER_MIN_IDLE_SECS + 60),
            min_idle_seconds: RECLAIMABLE_OWNER_MIN_IDLE_SECS,
        }
    }

    #[test]
    fn reclaim_zombie_is_reclaimable_regardless_of_other_signals() {
        // A zombie holds nothing; it is reclaimable even if it looks "responsive"
        // or has an intact executable or unknown age.
        let ev = ReclaimableOwnerEvidence {
            pid: 42,
            zombie: true,
            executable_deleted: false,
            mailbox_responsive: true,
            age_seconds: None,
            min_idle_seconds: RECLAIMABLE_OWNER_MIN_IDLE_SECS,
        };
        assert_eq!(ev.verdict(), ReclaimVerdict::ZombieHolder);
        assert!(ev.is_reclaimable());
    }

    #[test]
    fn reclaim_wedged_deleted_binary_is_reclaimable() {
        let ev = wedged_evidence(2860);
        assert_eq!(ev.verdict(), ReclaimVerdict::WedgedDeletedExecutable);
        assert!(ev.is_reclaimable());
    }

    #[test]
    fn reclaim_responsive_owner_is_never_reclaimable() {
        // The healthy in-place-upgraded server: deleted exe, old, but STILL
        // serving. Must never be reclaimed — that is the whole safety point.
        let mut ev = wedged_evidence(2860);
        ev.mailbox_responsive = true;
        assert_eq!(ev.verdict(), ReclaimVerdict::NotReclaimable);
        assert!(!ev.is_reclaimable());
    }

    #[test]
    fn reclaim_intact_executable_is_not_reclaimable() {
        // No deleted-executable signal and not a zombie: never reclaimable, even
        // if unresponsive and old (could be a healthy stdio server mid-hang that
        // a supervised restart should handle, not a lock steal).
        let mut ev = wedged_evidence(2860);
        ev.executable_deleted = false;
        assert_eq!(ev.verdict(), ReclaimVerdict::NotReclaimable);
    }

    #[test]
    fn reclaim_requires_idle_past_threshold() {
        let mut ev = wedged_evidence(2860);
        ev.age_seconds = Some(RECLAIMABLE_OWNER_MIN_IDLE_SECS - 1);
        assert_eq!(ev.verdict(), ReclaimVerdict::NotReclaimable);
    }

    #[test]
    fn reclaim_unknown_age_is_not_reclaimable() {
        // Conservative: an unknowable age is treated as too-fresh-to-judge.
        let mut ev = wedged_evidence(2860);
        ev.age_seconds = None;
        assert_eq!(ev.verdict(), ReclaimVerdict::NotReclaimable);
    }

    #[test]
    fn owner_set_reclaimable_requires_nonempty_and_unanimous() {
        assert!(
            !owner_set_is_reclaimable(&[]),
            "empty set is never reclaimable"
        );
        assert!(owner_set_is_reclaimable(&[wedged_evidence(1)]));
        assert!(owner_set_is_reclaimable(&[
            wedged_evidence(1),
            wedged_evidence(2)
        ]));

        // One live, non-reclaimable holder poisons the whole set: quarantining a
        // shared lock would steal it from a working process.
        let mut live = wedged_evidence(3);
        live.mailbox_responsive = true;
        assert!(!owner_set_is_reclaimable(&[wedged_evidence(1), live]));
    }

    #[test]
    fn reclaim_verdict_tokens_stable() {
        assert_eq!(ReclaimVerdict::ZombieHolder.as_str(), "zombie_holder");
        assert_eq!(
            ReclaimVerdict::WedgedDeletedExecutable.as_str(),
            "wedged_deleted_executable"
        );
        assert_eq!(ReclaimVerdict::NotReclaimable.as_str(), "not_reclaimable");
    }
}
