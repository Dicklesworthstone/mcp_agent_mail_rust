//! ATC learning invariants, performance budgets, and conservative fallback contract (br-0qt6e.1.5).
//!
//! This module defines the engineering guardrails for the ATC learning stack.
//! Every bead downstream of br-0qt6e.1.5 must satisfy these contracts. If a
//! change cannot meet these budgets, it must not ship.
//!
//! # Contract Coverage
//!
//! The contract covers four path families:
//!
//! 1. **Acted paths** — decisions that result in effects (advisory, release, probe)
//! 2. **Deliberate no-action paths** — decisions where the optimal action is "do nothing"
//! 3. **Safety-gated suppression paths** — safe mode or uncertainty gating blocks an action
//! 4. **Stale/partial-state paths** — operator surfaces display degraded data
//!
//! # How to Use
//!
//! - Performance tests (br-0qt6e.5.4) verify [`HOT_PATH_BUDGETS`] with instrumented ticks
//! - Property tests (br-0qt6e.5.2) verify [`INVARIANTS`] hold under random input
//! - Operator surfaces (br-0qt6e.4.*) check [`FRESHNESS_CONTRACTS`] before rendering
//! - Adaptation logic (br-0qt6e.3.*) checks [`ROLLBACK_TRIGGERS`] before committing changes
//! - All paths check [`FALSE_ACTION_BUDGETS`] before accepting a policy update

use serde::Serialize;

// ──────────────────────────────────────────────────────────────────────
// Hot-path performance budgets
// ──────────────────────────────────────────────────────────────────────

/// Performance budget for a single hot-path operation.
///
/// Budgets are specified in microseconds. The `p99_micros` field is the
/// maximum latency at the 99th percentile that the operation may consume.
/// The `max_micros` field is the absolute hard ceiling — exceeding this
/// triggers a budget-exceeded warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LatencyBudget {
    /// Operation name (for logging and alerting).
    pub operation: &'static str,
    /// Hot-path category.
    pub path: HotPathCategory,
    /// P99 latency budget in microseconds.
    pub p99_micros: u64,
    /// Absolute maximum latency in microseconds. Exceeding this is a bug.
    pub max_micros: u64,
    /// Maximum heap allocation in bytes per invocation (0 = no allocation allowed).
    pub max_alloc_bytes: u64,
    /// Whether this operation may perform I/O (disk, network).
    pub io_allowed: bool,
}

/// Categories for hot-path operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HotPathCategory {
    /// Per-tick decision loop (runs every tick, ~5ms budget total).
    TickLoop,
    /// Event resolution (runs on each outcome event).
    EventResolution,
    /// Summary read for operator surfaces (TUI, robot, web).
    SummaryRead,
    /// Operator surface refresh cycle.
    SurfaceRefresh,
    /// Experience append path (writing to `SQLite`).
    ExperienceAppend,
    /// Rollup incremental update.
    RollupUpdate,
}

/// All hot-path performance budgets for the ATC learning stack.
///
/// These budgets constrain what the learning stack may do on each path.
/// Performance tests must verify these with instrumented measurements.
pub const HOT_PATH_BUDGETS: &[LatencyBudget] = &[
    // ── Tick loop operations ───────────────────────────────────────
    LatencyBudget {
        operation: "tick.decision_logging",
        path: HotPathCategory::TickLoop,
        p99_micros: 200,
        max_micros: 500,
        max_alloc_bytes: 4096,
        io_allowed: false, // Decision logging writes to an in-memory buffer, not disk
    },
    LatencyBudget {
        operation: "tick.posterior_update",
        path: HotPathCategory::TickLoop,
        p99_micros: 50,
        max_micros: 200,
        max_alloc_bytes: 0, // In-place update, no allocation
        io_allowed: false,
    },
    LatencyBudget {
        operation: "tick.entropy_scheduling",
        path: HotPathCategory::TickLoop,
        p99_micros: 100,
        max_micros: 300,
        max_alloc_bytes: 2048, // Probe candidate list
        io_allowed: false,
    },
    LatencyBudget {
        operation: "tick.experience_capture",
        path: HotPathCategory::TickLoop,
        p99_micros: 300,
        max_micros: 800,
        max_alloc_bytes: 8192, // Experience row serialization
        io_allowed: false,     // Captured to in-memory buffer, flushed asynchronously
    },
    LatencyBudget {
        operation: "tick.total",
        path: HotPathCategory::TickLoop,
        p99_micros: 4_000,
        max_micros: 5_000, // Hard ceiling = baseline tick budget
        max_alloc_bytes: 32_768,
        io_allowed: false, // Tick loop must be non-blocking
    },
    // ── Event resolution ───────────────────────────────────────────
    LatencyBudget {
        operation: "resolution.outcome_match",
        path: HotPathCategory::EventResolution,
        p99_micros: 100,
        max_micros: 500,
        max_alloc_bytes: 1024,
        io_allowed: false,
    },
    LatencyBudget {
        operation: "resolution.state_transition",
        path: HotPathCategory::EventResolution,
        p99_micros: 50,
        max_micros: 200,
        max_alloc_bytes: 0,
        io_allowed: false,
    },
    LatencyBudget {
        operation: "resolution.rollup_increment",
        path: HotPathCategory::RollupUpdate,
        p99_micros: 200,
        max_micros: 500,
        max_alloc_bytes: 512,
        io_allowed: true, // Single-row SQLite UPDATE
    },
    // ── Experience append ──────────────────────────────────────────
    LatencyBudget {
        operation: "append.experience_row",
        path: HotPathCategory::ExperienceAppend,
        p99_micros: 500,
        max_micros: 2_000,
        max_alloc_bytes: 4096,
        io_allowed: true, // SQLite INSERT
    },
    // ── Summary reads (operator surfaces) ──────────────────────────
    LatencyBudget {
        operation: "summary.rollup_read",
        path: HotPathCategory::SummaryRead,
        p99_micros: 500,
        max_micros: 2_000,
        max_alloc_bytes: 16_384,
        io_allowed: true, // SQLite SELECT on rollup table
    },
    LatencyBudget {
        operation: "summary.open_experience_scan",
        path: HotPathCategory::SummaryRead,
        p99_micros: 1_000,
        max_micros: 5_000,
        max_alloc_bytes: 32_768,
        io_allowed: true, // Indexed SELECT on partial index
    },
    LatencyBudget {
        operation: "surface.tui_refresh",
        path: HotPathCategory::SurfaceRefresh,
        p99_micros: 10_000,
        max_micros: 50_000,
        max_alloc_bytes: 65_536,
        io_allowed: true,
    },
    LatencyBudget {
        operation: "surface.robot_status",
        path: HotPathCategory::SurfaceRefresh,
        p99_micros: 5_000,
        max_micros: 20_000,
        max_alloc_bytes: 32_768,
        io_allowed: true,
    },
];

// ──────────────────────────────────────────────────────────────────────
// Structural invariants
// ──────────────────────────────────────────────────────────────────────

/// A structural invariant that the learning stack must maintain at all times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invariant {
    /// Short identifier for the invariant.
    pub id: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// The property that must hold (formal enough for a property test).
    pub property: &'static str,
    /// What breaks if this invariant is violated.
    pub violation_consequence: &'static str,
    /// How to verify this invariant (test strategy).
    pub verification: &'static str,
}

/// All structural invariants for the ATC learning stack.
pub const INVARIANTS: &[Invariant] = &[
    // ── Monotonicity invariants ────────────────────────────────────
    Invariant {
        id: "inv.monotonic_experience_id",
        description: "Experience IDs are monotonically increasing",
        property: "For any two experiences e1, e2: if e1.created_ts < e2.created_ts, \
                   then e1.experience_id < e2.experience_id",
        violation_consequence: "Open-experience scans may miss rows; resolution may \
                                process experiences out of order",
        verification: "Property test: generate random experience sequences, verify ID ordering",
    },
    Invariant {
        id: "inv.monotonic_card_id",
        description: "Transparency card IDs are monotonically increasing",
        property: "For any two cards c1, c2: if c1.timestamp < c2.timestamp, \
                   then c1.card_id < c2.card_id",
        violation_consequence: "Card replay may skip or duplicate cards",
        verification: "Property test: emit cards concurrently, verify ID ordering",
    },
    // ── Idempotent resolution ──────────────────────────────────────
    Invariant {
        id: "inv.idempotent_resolution",
        description: "Resolving an already-resolved experience is a no-op",
        property: "For any experience e in state Resolved|Censored|Expired: \
                   resolve(e, outcome) returns success without mutation",
        violation_consequence: "Duplicate outcome events corrupt the experience store; \
                                rollup counts are double-counted",
        verification: "Unit test: resolve the same experience twice, verify no state change",
    },
    Invariant {
        id: "inv.valid_state_transitions",
        description: "Experience state transitions follow the lifecycle DAG",
        property: "Planned→Dispatched→Executed→Open→{Resolved,Censored,Expired} are the \
                   only valid transitions; no backward transitions; terminal states are absorbing",
        violation_consequence: "Experience may re-enter open state after resolution, causing \
                                rollup corruption and unbounded open-experience growth",
        verification: "Property test: random transition sequences, verify validate_transition() rejects invalid ones",
    },
    // ── Bounded memory growth ──────────────────────────────────────
    Invariant {
        id: "inv.bounded_open_experiences",
        description: "Open experience count is bounded by active_agents × max_effects_per_tick",
        property: "At any point: count(state='open') ≤ active_agent_count × max_effects_per_tick × \
                   stale_timeout_ticks. Stale experiences are censored after timeout.",
        violation_consequence: "Unbounded growth of open experiences causes full-table scans \
                                on the partial index, exceeding summary read budgets",
        verification: "Integration test: run 100 agents for 1000 ticks, verify open count stays bounded",
    },
    Invariant {
        id: "inv.bounded_rollup_rows",
        description: "Rollup table row count is bounded by |subsystems| × |effect_kinds| × |risk_tiers|",
        property: "count(atc_experience_rollups) ≤ 3 × max_effect_kinds × max_risk_tiers. \
                   Currently: 3 subsystems × ~10 effects × 3 tiers = ~90 rows max.",
        violation_consequence: "Rollup reads exceed summary budget; operator surface latency spikes",
        verification: "Integration test: verify rollup row count after bulk experience insertion",
    },
    Invariant {
        id: "inv.bounded_regret_window",
        description: "Regret tracking window is bounded at 100 entries per action",
        property: "For any action a: len(regret_window[a]) ≤ 100. Older entries are evicted FIFO.",
        violation_consequence: "Regret window grows unbounded; PID tuner average regret becomes stale",
        verification: "Unit test: insert 200 regret entries, verify window size ≤ 100",
    },
    Invariant {
        id: "inv.bounded_regime_history",
        description: "CUSUM regime change history is bounded at 50 entries",
        property: "len(regime_history) ≤ 50. Oldest entries evicted on overflow.",
        violation_consequence: "Regime history grows unbounded; diagnostic reads slow down",
        verification: "Unit test: trigger 100 regime changes, verify history size ≤ 50",
    },
    // ── No accidental full-table scans ─────────────────────────────
    Invariant {
        id: "inv.no_raw_scan_on_user_path",
        description: "No user-facing path performs a full scan of atc_experiences",
        property: "All queries on atc_experiences used by TUI, robot, or web surfaces \
                   use an index (idx_atc_exp_open, idx_atc_exp_subject, idx_atc_exp_created, \
                   idx_atc_exp_stratum). EXPLAIN QUERY PLAN must show 'USING INDEX' or \
                   'USING COVERING INDEX', never 'SCAN TABLE'.",
        violation_consequence: "Operator surface latency scales linearly with experience \
                                table size instead of staying constant",
        verification: "E2E test: run EXPLAIN QUERY PLAN on all learning queries, assert no SCAN TABLE",
    },
    // ── Calibration safety ─────────────────────────────────────────
    Invariant {
        id: "inv.safe_mode_blocks_release",
        description: "Safe mode always blocks ReleaseReservations actions",
        property: "When safe_mode_active=true: no ReleaseReservations effect is executed. \
                   Advisory and Probe effects may continue.",
        violation_consequence: "Miscalibrated ATC releases active agents' file reservations, \
                                destroying work in progress",
        verification: "Property test: when safe_mode=true, verify no Release effect in output",
    },
    Invariant {
        id: "inv.conformal_gate_blocks_uncertain_release",
        description: "Conformal uncertainty gate blocks releases when interval is too wide",
        property: "When conformal_set.is_uncertain()=true for the release action: \
                   WithholdRelease is chosen instead of ReleaseReservations",
        violation_consequence: "ATC releases with false confidence when the prediction \
                                interval is too wide to distinguish alive from dead",
        verification: "Unit test: set conformal width > 10.0, verify WithholdRelease chosen",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Freshness and partial-data contracts
// ──────────────────────────────────────────────────────────────────────

/// Contract for how operator surfaces handle stale or partial data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FreshnessContract {
    /// Which surface or data source this contract applies to.
    pub surface: &'static str,
    /// Maximum acceptable staleness before the surface must display a warning.
    pub max_staleness_secs: u64,
    /// What the surface must display when data is stale.
    pub stale_behavior: &'static str,
    /// What the surface must display when data is partially available.
    pub partial_behavior: &'static str,
}

/// Freshness contracts for all operator surfaces consuming learning data.
pub const FRESHNESS_CONTRACTS: &[FreshnessContract] = &[
    FreshnessContract {
        surface: "tui.atc_dashboard",
        max_staleness_secs: 5,
        stale_behavior: "Display '⚠ ATC data stale ({N}s)' in header; dim all metric values",
        partial_behavior: "Display '◐ partial' next to incomplete rollup rows; show available \
                           data with a 'confidence: low' indicator",
    },
    FreshnessContract {
        surface: "robot.atc_status",
        max_staleness_secs: 10,
        stale_behavior: "Include 'staleness_warning': true in JSON output with \
                         'last_update_ts' field showing the actual last update time",
        partial_behavior: "Include 'partial_data': true and 'available_subsystems': [...] \
                           so consumers know which data is present",
    },
    FreshnessContract {
        surface: "web.atc_panel",
        max_staleness_secs: 15,
        stale_behavior: "Yellow banner: 'ATC learning data is {N}s stale — metrics may not \
                         reflect current state'",
        partial_behavior: "Grey out missing subsystem panels with 'No data yet' placeholder",
    },
    FreshnessContract {
        surface: "transparency_card_stream",
        max_staleness_secs: 30,
        stale_behavior: "Emit a synthetic 'heartbeat' card every 30s when no real cards are \
                         produced, so consumers can distinguish 'quiet' from 'broken'",
        partial_behavior: "Cards always include regime_context; if any field is unknown, \
                           populate with the last known value and set 'context_stale': true",
    },
    FreshnessContract {
        surface: "evidence_ledger",
        max_staleness_secs: 0, // Real-time append
        stale_behavior: "Ledger is append-only; staleness is not applicable",
        partial_behavior: "Ledger entries are self-contained; partial state is not possible \
                           per entry. Ring buffer capacity (1000) may cause oldest entries to \
                           be evicted — this is by design, not partial data.",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Conservative fallback policy
// ──────────────────────────────────────────────────────────────────────

/// A fallback rule that defines what the system does under degraded conditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FallbackRule {
    /// Condition that triggers the fallback.
    pub trigger: &'static str,
    /// What the system does under this condition.
    pub behavior: &'static str,
    /// Why this fallback is safe (what property it preserves).
    pub safety_argument: &'static str,
}

/// Conservative fallback rules for degraded conditions.
///
/// When confidence is low, evidence is stale, snapshots are partial, or
/// the system becomes too noisy, these rules define the safe behavior.
pub const FALLBACK_RULES: &[FallbackRule] = &[
    FallbackRule {
        trigger: "Sparse data: fewer than 30 observations in a conformal calibration window",
        behavior: "Conformal predictor returns None. No uncertainty-based gating occurs. \
                   Decisions proceed using prior-only posteriors.",
        safety_argument: "Prior-only posteriors are conservative (high entropy, low confidence). \
                          Safe mode is not triggered because sparse data is expected during \
                          cold start — the system has not yet earned calibration.",
    },
    FallbackRule {
        trigger: "Calibration drift: e-process e-value exceeds 20.0 OR CUSUM detects degradation",
        behavior: "Enter safe mode: block ReleaseReservations, continue Probe and Advisory. \
                   Require 20 consecutive correct predictions before exiting safe mode.",
        safety_argument: "Safe mode blocks the highest-damage action (releasing active agents' \
                          work). Probes continue because they are information-gathering and \
                          never cause harm. Advisory continues because informational messages \
                          are low-cost even if misdirected.",
    },
    FallbackRule {
        trigger: "Write pressure: experience append latency exceeds 2ms (p99 budget)",
        behavior: "Shed experience capture: buffer in memory (up to 1000 rows) and flush \
                   asynchronously. If buffer overflows, drop oldest uncommitted rows. \
                   Decision quality is preserved because decisions use in-memory posteriors, \
                   not the SQLite experience table.",
        safety_argument: "Decisions are made from in-memory posteriors (EWMA), not from the \
                          experience table. The experience table is for learning and audit — \
                          losing recent rows degrades learning quality but not decision safety.",
    },
    FallbackRule {
        trigger: "Broken calibration: posterior normalizes to zero (all states have probability floor)",
        behavior: "Return the default action for the subsystem (DeclareAlive for liveness, \
                   Ignore for conflict, RouteHere for load). Log the normalization failure. \
                   Do NOT enter safe mode for a single normalization failure (transient).",
        safety_argument: "Default actions are the least aggressive: DeclareAlive preserves \
                          agent work, Ignore does not intervene in conflicts, RouteHere \
                          maintains the current routing. All are safe no-ops.",
    },
    FallbackRule {
        trigger: "Incomplete execution truth: outcome event arrives but the experience is already resolved",
        behavior: "Return success without mutation (idempotent resolution). Log the duplicate \
                   for diagnostic purposes. Do not increment rollup counters.",
        safety_argument: "Idempotent resolution prevents double-counting in rollups. Duplicate \
                          events are expected from retries, replays, and concurrent observers.",
    },
    FallbackRule {
        trigger: "Noisy system: advisory rate exceeds 2× the baseline per-agent per-hour rate",
        behavior: "Activate advisory cooldown (300s per agent). Log the noise event. If noise \
                   persists for >10 consecutive ticks, escalate to safe mode.",
        safety_argument: "Excessive advisories degrade operator trust. Cooldown reduces noise \
                          while preserving the ability to detect genuine issues. Safe mode \
                          escalation ensures the root cause (miscalibration) is addressed.",
    },
    FallbackRule {
        trigger: "Population model lock poisoned",
        behavior: "Fall back to 'unknown' program defaults (300s mean, 0.5×mean variance). \
                   Log the poisoning event. Do NOT attempt lock recovery.",
        safety_argument: "The default prior (300s) is conservative — it triggers suspicion \
                          later rather than earlier, preventing false positives for new agents \
                          at the cost of slower detection of genuine issues.",
    },
    FallbackRule {
        trigger: "PID tuner produces loss matrix entry outside bounds [0.1×original, 10×original]",
        behavior: "Clamp to the nearest bound. Log the clamping event via transparency card. \
                   If clamping occurs for >20 consecutive updates, reset PID integral to zero \
                   and loss entry to original value.",
        safety_argument: "Bounds prevent runaway adaptation. The 10× ceiling ensures no action \
                          becomes overwhelmingly expensive; the 0.1× floor ensures no action \
                          becomes trivially cheap (which would disable the safety margin).",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Rollback triggers
// ──────────────────────────────────────────────────────────────────────

/// Conditions that trigger automatic rollback of a behavior-changing policy update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RollbackTrigger {
    /// What is being rolled back.
    pub target: &'static str,
    /// Condition that triggers the rollback.
    pub condition: &'static str,
    /// What the rollback restores.
    pub rollback_to: &'static str,
    /// Non-regression obligation: what must not get worse after the rollback.
    pub non_regression: &'static str,
}

/// Automatic rollback triggers for behavior-changing policy updates.
pub const ROLLBACK_TRIGGERS: &[RollbackTrigger] = &[
    RollbackTrigger {
        target: "loss_matrix_adaptation",
        condition: "Adapted loss matrix produces higher average regret than the baseline \
                    fixed matrix over a 200-decision window",
        rollback_to: "Baseline loss matrix from atc_baseline.rs. PID integral reset to zero.",
        non_regression: "Regret must not exceed 110% of baseline regret for the same decision \
                         sequence. Tick latency must not exceed baseline tick budget (5ms).",
    },
    RollbackTrigger {
        target: "shadow_policy_promotion",
        condition: "Promoted policy causes e-process to trigger within 50 ticks of promotion",
        rollback_to: "Previous active policy. Shadow policy reverted to shadow status.",
        non_regression: "E-process must stay below 50% of alert threshold after rollback. \
                         Advisory noise rate must not increase.",
    },
    RollbackTrigger {
        target: "adaptive_threshold_drift",
        condition: "Per-agent effective_k reaches min or max bound for >100 consecutive observations",
        rollback_to: "Reset to base_k=3.0. Clear Beta-Binomial counters (α=β=2).",
        non_regression: "False-positive suspicion rate must stay below 2%. False-negative \
                         (missed dead agent) rate must stay below 1%.",
    },
    RollbackTrigger {
        target: "conformal_window_corruption",
        condition: "Empirical coverage drops below 75% (nominal 90%) for >200 consecutive observations",
        rollback_to: "Clear conformal calibration window. Return to sparse-data fallback \
                       (no uncertainty gating) until window refills.",
        non_regression: "No ReleaseReservations executed while conformal window is refilling \
                         (safe mode should be active due to concurrent e-process trigger).",
    },
];

// ──────────────────────────────────────────────────────────────────────
// False-action and noise budgets
// ──────────────────────────────────────────────────────────────────────

/// Budget for false actions, silent failures, user noise, and diagnostic completeness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionBudget {
    /// What this budget constrains.
    pub category: &'static str,
    /// Concrete numerical bound.
    pub bound: &'static str,
    /// Measurement methodology (how to verify).
    pub measurement: &'static str,
    /// What happens if the budget is exceeded.
    pub exceeded_action: &'static str,
}

/// Budgets that downstream math, surfaces, and tests must respect.
pub const FALSE_ACTION_BUDGETS: &[ActionBudget] = &[
    ActionBudget {
        category: "false_positive_release",
        bound: "0 per 10,000 decisions. Zero tolerance for releasing an active agent's reservations.",
        measurement: "Count ReleaseReservations effects where the agent was actually alive \
                      (determined by subsequent activity within 60s of release).",
        exceeded_action: "Immediate safe mode entry. Loss matrix ReleaseAlive entry increased to \
                          200 (2× baseline). Transparency card emitted. Alert escalated to operator.",
    },
    ActionBudget {
        category: "false_positive_advisory",
        bound: "≤5% of advisories sent to agents that did not need intervention \
                (agent was active and not in conflict).",
        measurement: "Count Advisory effects where agent showed activity within 30s (liveness) \
                      or no actual file conflict existed (conflict).",
        exceeded_action: "Increase advisory cooldown to 600s. Log noise escalation. \
                          If sustained for >1 hour, enter advisory-suppression mode.",
    },
    ActionBudget {
        category: "silent_failure",
        bound: "0 silent failures per session. Every decision that fails to execute must \
                produce a diagnostic artifact (transparency card or evidence ledger entry).",
        measurement: "Audit trail: for every decision in evidence ledger, verify there is a \
                      corresponding execution result or non-execution reason.",
        exceeded_action: "Create issue. Silent failures indicate a bug in the capture pipeline.",
    },
    ActionBudget {
        category: "user_noise",
        bound: "≤10 advisories per agent per hour under steady-state operation. \
                ≤3 toast notifications per minute in the TUI.",
        measurement: "Count advisories sent per agent per rolling hour. Count toasts displayed \
                      per rolling minute.",
        exceeded_action: "Activate advisory cooldown escalation. Reduce TUI toast severity \
                          threshold to 'warning' (suppress 'info' toasts).",
    },
    ActionBudget {
        category: "diagnostic_artifact_completeness",
        bound: "Every resolved experience must have: (1) decision_id, (2) effect_id, \
                (3) outcome_json, (4) features_json, (5) resolved_ts. Missing fields \
                indicate capture pipeline bugs.",
        measurement: "Query: SELECT COUNT(*) FROM atc_experiences WHERE state IN ('resolved') \
                      AND (outcome_json IS NULL OR features_json IS NULL OR resolved_ts IS NULL). \
                      Must return 0.",
        exceeded_action: "Create issue. Missing fields degrade learning quality and make \
                          audit trails incomplete.",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Proof obligations for future code and tests
// ──────────────────────────────────────────────────────────────────────

/// A proof obligation that future code and tests must satisfy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofObligation {
    /// Obligation identifier.
    pub id: &'static str,
    /// What must be proven.
    pub obligation: &'static str,
    /// Which bead(s) must satisfy this obligation.
    pub responsible_beads: &'static [&'static str],
    /// How to verify (test type: unit, property, integration, E2E).
    pub test_type: &'static str,
}

/// Proof obligations that downstream beads must satisfy.
pub const PROOF_OBLIGATIONS: &[ProofObligation] = &[
    ProofObligation {
        id: "proof.no_raw_scan",
        obligation: "All learning queries use indexed access (EXPLAIN QUERY PLAN shows \
                     USING INDEX, never SCAN TABLE on atc_experiences)",
        responsible_beads: &["br-0qt6e.5.4"],
        test_type: "E2E: run EXPLAIN QUERY PLAN on all queries",
    },
    ProofObligation {
        id: "proof.idempotent_resolution",
        obligation: "Resolving the same experience twice produces identical state and \
                     does not increment rollup counters",
        responsible_beads: &["br-0qt6e.5.2"],
        test_type: "Property: generate random resolution sequences with duplicates",
    },
    ProofObligation {
        id: "proof.bounded_open",
        obligation: "Open experience count never exceeds active_agents × 10 × stale_timeout_ticks",
        responsible_beads: &["br-0qt6e.5.2", "br-0qt6e.5.3"],
        test_type: "Integration: simulate 100 agents × 1000 ticks",
    },
    ProofObligation {
        id: "proof.safe_mode_blocks_release",
        obligation: "No ReleaseReservations effect is executed when safe_mode=true",
        responsible_beads: &["br-0qt6e.5.2"],
        test_type: "Property: random tick sequences with safe_mode=true, verify no Release",
    },
    ProofObligation {
        id: "proof.tick_budget",
        obligation: "P99 tick latency stays within 5ms (5000μs) budget with learning enabled",
        responsible_beads: &["br-0qt6e.5.4"],
        test_type: "Performance: instrumented 10,000 ticks, measure P99 latency",
    },
    ProofObligation {
        id: "proof.transparency_completeness",
        obligation: "Every loss matrix change, safe mode transition, and regime shift \
                     produces a transparency card with all required fields",
        responsible_beads: &["br-0qt6e.5.2", "br-0qt6e.5.3"],
        test_type: "Integration: trigger all card-emitting events, verify card completeness",
    },
    ProofObligation {
        id: "proof.rollback_non_regression",
        obligation: "After every automatic rollback, the non-regression metric is verified \
                     within 50 ticks. If violated, escalate to operator.",
        responsible_beads: &["br-0qt6e.5.3"],
        test_type: "E2E: trigger rollback scenario, verify non-regression holds",
    },
    ProofObligation {
        id: "proof.zero_false_release",
        obligation: "False-positive release rate is exactly 0 across all test scenarios",
        responsible_beads: &["br-0qt6e.5.1", "br-0qt6e.5.2", "br-0qt6e.5.3"],
        test_type: "All: synthetic corpus, property tests, and E2E must verify zero false releases",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Query helpers
// ──────────────────────────────────────────────────────────────────────

/// Look up a hot-path budget by operation name.
#[must_use]
pub fn budget_for(operation: &str) -> Option<&'static LatencyBudget> {
    HOT_PATH_BUDGETS.iter().find(|b| b.operation == operation)
}

/// Look up an invariant by ID.
#[must_use]
pub fn invariant_by_id(id: &str) -> Option<&'static Invariant> {
    INVARIANTS.iter().find(|i| i.id == id)
}

/// Check if a measured latency exceeds its budget. Returns (exceeded, budget).
#[must_use]
pub fn check_budget(
    operation: &str,
    measured_micros: u64,
) -> Option<(bool, &'static LatencyBudget)> {
    budget_for(operation).map(|b| (measured_micros > b.max_micros, b))
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_path_budgets_consistent() {
        for budget in HOT_PATH_BUDGETS {
            assert!(
                budget.p99_micros <= budget.max_micros,
                "Budget {}: p99 ({}) exceeds max ({})",
                budget.operation,
                budget.p99_micros,
                budget.max_micros
            );
        }
    }

    #[test]
    fn tick_total_budget_matches_baseline() {
        let tick_total = budget_for("tick.total").expect("tick.total budget must exist");
        assert_eq!(
            tick_total.max_micros, 5_000,
            "tick total must match baseline 5ms"
        );
    }

    #[test]
    fn tick_components_fit_within_total() {
        let tick_ops: Vec<&LatencyBudget> = HOT_PATH_BUDGETS
            .iter()
            .filter(|b| matches!(b.path, HotPathCategory::TickLoop) && b.operation != "tick.total")
            .collect();
        let component_sum: u64 = tick_ops.iter().map(|b| b.p99_micros).sum();
        let tick_total = budget_for("tick.total").unwrap();
        assert!(
            component_sum <= tick_total.p99_micros,
            "Tick component p99 sum ({component_sum}μs) exceeds tick total p99 ({}μs)",
            tick_total.p99_micros
        );
    }

    #[test]
    fn invariant_ids_unique() {
        let mut ids: Vec<&str> = INVARIANTS.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(len_before, ids.len(), "Duplicate invariant ID found");
    }

    #[test]
    fn all_invariants_have_verification() {
        for inv in INVARIANTS {
            assert!(
                !inv.verification.is_empty(),
                "Invariant {} has no verification strategy",
                inv.id
            );
        }
    }

    #[test]
    fn freshness_contracts_non_empty() {
        assert!(!FRESHNESS_CONTRACTS.is_empty());
        for contract in FRESHNESS_CONTRACTS {
            assert!(!contract.stale_behavior.is_empty());
            assert!(!contract.partial_behavior.is_empty());
        }
    }

    #[test]
    fn fallback_rules_cover_all_degraded_conditions() {
        // Verify key degraded conditions are covered
        let triggers: Vec<&str> = FALLBACK_RULES.iter().map(|r| r.trigger).collect();
        assert!(triggers.iter().any(|t| t.contains("Sparse data")));
        assert!(triggers.iter().any(|t| t.contains("Calibration drift")));
        assert!(triggers.iter().any(|t| t.contains("Write pressure")));
        assert!(triggers.iter().any(|t| t.contains("Broken calibration")));
        assert!(triggers.iter().any(|t| t.contains("Incomplete execution")));
        assert!(triggers.iter().any(|t| t.contains("Noisy system")));
    }

    #[test]
    fn rollback_triggers_have_non_regression() {
        for trigger in ROLLBACK_TRIGGERS {
            assert!(
                !trigger.non_regression.is_empty(),
                "Rollback trigger for {} has no non-regression obligation",
                trigger.target
            );
        }
    }

    #[test]
    fn false_action_budgets_cover_key_categories() {
        let categories: Vec<&str> = FALSE_ACTION_BUDGETS.iter().map(|b| b.category).collect();
        assert!(categories.contains(&"false_positive_release"));
        assert!(categories.contains(&"false_positive_advisory"));
        assert!(categories.contains(&"silent_failure"));
        assert!(categories.contains(&"user_noise"));
        assert!(categories.contains(&"diagnostic_artifact_completeness"));
    }

    #[test]
    fn proof_obligations_reference_existing_beads() {
        for obligation in PROOF_OBLIGATIONS {
            for bead in obligation.responsible_beads {
                assert!(
                    bead.starts_with("br-"),
                    "Proof obligation {} references invalid bead '{}'",
                    obligation.id,
                    bead
                );
            }
        }
    }

    #[test]
    fn check_budget_works() {
        let (exceeded, _) = check_budget("tick.total", 6_000).unwrap();
        assert!(exceeded, "6000μs should exceed tick.total max of 5000μs");

        let (exceeded, _) = check_budget("tick.total", 3_000).unwrap();
        assert!(
            !exceeded,
            "3000μs should not exceed tick.total max of 5000μs"
        );

        assert!(check_budget("nonexistent.op", 100).is_none());
    }
}
