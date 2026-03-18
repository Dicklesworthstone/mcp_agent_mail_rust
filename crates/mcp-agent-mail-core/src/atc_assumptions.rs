//! Assumptions ledger, EV gates, and transparency cards for ATC learning (br-0qt6e.1.7).
//!
//! This module is the **reasoning contract** for the ATC learning stack. It
//! records *why* each mathematical method exists, *what assumptions* it makes,
//! *what evidence* would justify promoting or retiring it, and *how* operators
//! can inspect decisions after the fact.
//!
//! # Contract Scope
//!
//! Every mathematical method family used by ATC must have an entry in the
//! assumptions ledger ([`ASSUMPTIONS_LEDGER`]). Every entry carries an EV gate
//! ([`EvGate`]) that ties the method to a concrete user value. And every
//! material policy change must produce a transparency card ([`TransparencyCard`])
//! that operators can inspect in the TUI, robot output, or audit log.
//!
//! # How to Use This Module
//!
//! 1. **Before adding a new method**: Check [`ASSUMPTIONS_LEDGER`] for an
//!    existing family that already covers the need. If none applies, add a
//!    new [`AssumptionEntry`] with a complete EV gate before writing any code.
//!
//! 2. **Before promoting a method to production**: Verify that the evidence
//!    in [`EvGate::promotion_evidence`] has been collected and documented.
//!    If it has not, the method must remain behind a feature gate or shadow
//!    mode.
//!
//! 3. **Before retiring a method**: Check [`EvGate::sunset_evidence`] for the
//!    retirement trigger. If the evidence is present, remove the method and
//!    update the ledger entry's status to [`MethodStatus::Retired`].
//!
//! 4. **When a transparency card is emitted**: Verify it carries all required
//!    fields defined in [`TransparencyCard`]. Incomplete cards are a bug.
//!
//! # Anti-Complexity Guarantee
//!
//! The EV gate is the enforcing mechanism against math-for-math's-sake
//! complexity. A method that cannot fill in *all* fields of its [`EvGate`]
//! — especially `user_value`, `failure_it_prevents`, and `what_happens_without`
//! — is not justified and must not be added.

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// Method status lifecycle
// ──────────────────────────────────────────────────────────────────────

/// Lifecycle status of a mathematical method family in the ATC stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MethodStatus {
    /// Active and running in the production decision path.
    Active,
    /// Implemented and available but not currently wired into live decisions.
    /// Used for methods that have code paths but are not yet consumed.
    Available,
    /// Running in shadow mode (decisions logged but not acted upon).
    Shadow,
    /// Behind a feature gate; requires explicit opt-in.
    Gated,
    /// Retired: evidence showed it was no longer earning its keep.
    Retired,
    /// Placeholder: field exists in data structures but not used in logic.
    Placeholder,
}

// ──────────────────────────────────────────────────────────────────────
// EV gate — the justification contract for each method
// ──────────────────────────────────────────────────────────────────────

/// Expected-value gate for a mathematical method family.
///
/// Every method must demonstrate concrete user value. The gate ties the
/// method to a specific failure mode it prevents, the operator outcome it
/// improves, and what happens if the method is removed.
///
/// **Anti-complexity rule**: If any field is "N/A" or vague ("improves
/// accuracy"), the method is not justified. Be concrete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvGate {
    /// One-sentence description of what user-visible outcome this method
    /// improves. Must reference a specific operator experience, not an
    /// abstract metric.
    ///
    /// Good: "Prevents false-positive reservation releases that destroy
    /// agent work in progress."
    ///
    /// Bad: "Improves prediction accuracy."
    pub user_value: &'static str,

    /// The specific failure mode this method is designed to prevent.
    /// Must be a concrete scenario, not a category.
    pub failure_it_prevents: &'static str,

    /// What happens to users if this method is removed entirely.
    /// Must describe the observable degradation, not the technical one.
    pub what_happens_without: &'static str,

    /// What evidence would justify *promoting* this method from shadow/gated
    /// to active production use. Must be falsifiable and measurable.
    pub promotion_evidence: &'static str,

    /// What evidence would justify *retiring* this method — removing it
    /// entirely because it is no longer earning its keep. Must describe a
    /// concrete trigger, not "when we decide to."
    pub sunset_evidence: &'static str,

    /// Risk tier: how dangerous is it if this method misbehaves?
    /// 0 = informational (e.g., entropy scheduling — bad probes are annoying
    ///     but not destructive).
    /// 1 = moderate (e.g., rhythm tracking — false suspicion triggers
    ///     unnecessary probes).
    /// 2 = high (e.g., release gating — false positive destroys work;
    ///     false negative leaves dead reservations).
    pub risk_tier: u8,
}

// ──────────────────────────────────────────────────────────────────────
// Assumption entry — the per-method contract
// ──────────────────────────────────────────────────────────────────────

/// A single entry in the assumptions ledger.
///
/// Each entry documents one mathematical method family: what it assumes,
/// when those assumptions fail, and how the system degrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssumptionEntry {
    /// Unique identifier for this method family. Used in transparency cards
    /// and audit logs. Format: `atc.{subsystem}.{method}`.
    pub method_id: &'static str,

    /// Human-readable name.
    pub name: &'static str,

    /// Mathematical family (e.g., "Bayesian", "Sequential Testing",
    /// "Control Theory", "Graph Algorithm").
    pub family: &'static str,

    /// Which ATC subsystem(s) use this method.
    pub subsystems: &'static [&'static str],

    /// Current lifecycle status.
    pub status: MethodStatus,

    /// The EV gate that justifies this method's existence.
    pub ev_gate: EvGate,

    /// Enumerated assumptions this method makes. Each is a concrete,
    /// falsifiable statement — not a textbook disclaimer.
    pub assumptions: &'static [&'static str],

    /// Signals that indicate one or more assumptions are violated and the
    /// method's output should be treated as suspect. Each signal maps to
    /// a monitoring check that operators or automated tests can run.
    pub invalidation_signals: &'static [&'static str],

    /// What the system does when this method fails or its assumptions are
    /// violated. Must describe the concrete fallback, not "degrades
    /// gracefully."
    pub degradation_behavior: &'static str,

    /// Interaction constraints with other methods. Lists `method_id`s that
    /// this method must not contradict or that must run before/after.
    pub interaction_constraints: &'static [&'static str],
}

// ──────────────────────────────────────────────────────────────────────
// Transparency card — the audit artifact for policy changes
// ──────────────────────────────────────────────────────────────────────

/// Transparency card emitted for every material ATC policy change or
/// mathematical decision.
///
/// Transparency cards are the operator-facing audit record. They answer
/// "what changed, why, under what evidence, and who approved it." Future
/// audit, operator-surface, and archive beads consume these cards directly
/// — no second format needed.
///
/// # Required Fields
///
/// All fields are required. A card with any empty required field is a bug.
/// Optional fields are wrapped in `Option<T>`.
///
/// # Emission Points
///
/// Cards are emitted when:
/// 1. A loss matrix entry is adjusted by the PID tuner.
/// 2. Safe mode is entered or exited.
/// 3. A regime change is detected by CUSUM.
/// 4. A conformal uncertainty gate blocks a release.
/// 5. The adaptive mode controller transitions between modes.
/// 6. A shadow policy is promoted to active.
/// 7. A method family is enabled, disabled, or retired.
///
/// # Storage
///
/// Cards are appended to the evidence ledger (JSONL) and optionally
/// surfaced in the TUI timeline, robot output, and Git audit artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransparencyCard {
    /// Monotonic card ID (assigned by emitter).
    pub card_id: u64,

    /// ISO-8601 timestamp of the policy change.
    pub timestamp_iso: String,

    /// Which method produced this card (references `AssumptionEntry::method_id`).
    pub method_id: String,

    /// Policy ID affected (e.g., `liveness.loss_matrix`, `calibration.safe_mode`).
    pub policy_id: String,

    /// Evidence ID that triggered this change (from evidence ledger).
    pub evidence_id: String,

    /// Regime context at the time of the change.
    pub regime_context: RegimeContext,

    /// What changed: before and after values in human-readable form.
    pub change_description: String,

    /// Why this change was made. Must be a concrete sentence referencing
    /// specific evidence, not "to improve accuracy."
    pub rationale: String,

    /// The decision's expected loss before and after the change.
    pub expected_loss_before: f64,
    pub expected_loss_after: f64,

    /// Whether this change was automatic (PID, threshold) or operator-triggered.
    pub trigger: CardTrigger,

    /// Optional: the experience IDs that provided evidence for this change.
    pub evidence_experience_ids: Vec<String>,

    /// Optional: if this card reverses a previous card, reference its ID.
    pub reverses_card_id: Option<u64>,
}

/// How a transparency card was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardTrigger {
    /// Automatic: triggered by the learning/adaptation loop.
    Automatic,
    /// Operator: triggered by an explicit operator command.
    Operator,
    /// Rollback: triggered by a safety gate (safe mode, uncertainty).
    Rollback,
    /// Promotion: shadow policy promoted to active.
    Promotion,
    /// Retirement: method disabled or removed.
    Retirement,
}

/// Regime context snapshot captured at the time of a policy change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeContext {
    /// Current adaptive mode (Nominal, Pressure, Conservative).
    pub adaptive_mode: String,
    /// Whether safe mode is active.
    pub safe_mode_active: bool,
    /// Current e-process e-value.
    pub eprocess_value: f64,
    /// Current CUSUM state (positive accumulator).
    pub cusum_s_pos: f64,
    /// Current CUSUM state (negative accumulator).
    pub cusum_s_neg: f64,
    /// Number of active agents.
    pub active_agent_count: u32,
    /// Tick number at which this card was emitted.
    pub tick_number: u64,
}

// ──────────────────────────────────────────────────────────────────────
// Escalation and simplification criteria
// ──────────────────────────────────────────────────────────────────────

/// Criteria for escalating mathematical sophistication.
///
/// New mathematical mechanisms may only be added when ALL of these
/// conditions are met. This prevents decorative complexity.
pub const ESCALATION_CRITERIA: &[&str] = &[
    "An existing failure mode causes user-visible harm (false releases, missed deadlocks, excessive noise)",
    "The harm is quantifiable via regret tracking, experience resolution, or operator feedback",
    "No simpler mechanism (threshold, heuristic, existing method family) addresses the failure adequately",
    "The proposed mechanism has a falsifiable promotion_evidence gate in the EV gate",
    "The proposed mechanism has a concrete sunset_evidence trigger for future retirement",
    "The mechanism's hot-path cost fits within the tick budget (5ms baseline, see atc_baseline)",
    "The mechanism's storage cost fits within the retention budget (see br-0qt6e.1.4)",
    "At least one synthetic scenario in the test corpus exercises the mechanism's failure mode",
];

/// Criteria for simplifying (disabling or removing) a method.
///
/// A method should be simplified or retired when ANY of these conditions
/// is met.
pub const SIMPLIFICATION_CRITERIA: &[&str] = &[
    "The method's regret contribution is indistinguishable from a constant-action baseline over 1000+ decisions",
    "The method's conformal interval width exceeds the decision margin (the method adds noise, not signal)",
    "The method has been in shadow mode for >30 days with no operator or automated test discovering its absence",
    "The method's assumption-invalidation signals fire more than 10% of the time in production",
    "The method's hot-path cost exceeds 20% of the tick budget without proportional regret reduction",
    "An operator explicitly flags the method as harmful and provides evidence of false-positive impact",
];

// ──────────────────────────────────────────────────────────────────────
// The assumptions ledger — one entry per method family
// ──────────────────────────────────────────────────────────────────────

/// The complete assumptions ledger for all mathematical method families
/// in the ATC learning stack.
///
/// This is the canonical reference. Audit, operator, and testing beads
/// should read from this array, not from ad-hoc documentation.
pub const ASSUMPTIONS_LEDGER: &[AssumptionEntry] = &[
    // ── 1. Expected-Loss Decision Theory ────────────────────────────
    AssumptionEntry {
        method_id: "atc.core.expected_loss",
        name: "Expected-Loss Decision Theory",
        family: "Decision Theory",
        subsystems: &["liveness", "conflict", "load_routing"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Forces every ATC action to quantify its cost against \
                         every possible world-state, preventing ad-hoc heuristics \
                         that silently favor one failure mode over another.",
            failure_it_prevents: "An ATC action that is cheap under one state \
                                  (e.g., release an idle agent) but catastrophic \
                                  under another (e.g., release an agent mid-edit) \
                                  is selected without weighing both outcomes.",
            what_happens_without: "ATC falls back to threshold-based heuristics. \
                                   False-positive reservation releases increase \
                                   because there is no explicit cost for the \
                                   'release alive agent' failure mode.",
            promotion_evidence: "N/A — this is the foundational decision framework. \
                                 It is active by construction.",
            sunset_evidence: "If a simpler decision rule (e.g., majority vote) \
                              achieves equal or lower regret over 10,000+ decisions \
                              in a synthetic scenario corpus, the loss-matrix \
                              framework may be replaced.",
            risk_tier: 2,
        },
        assumptions: &[
            "State and action spaces are discrete and finite (3 states × 3 actions per subsystem)",
            "Loss matrices are specified a priori and time-invariant within a regime (PID tuning adjusts slowly)",
            "The posterior P(state|evidence) is well-calibrated (coverage ≥ 85%)",
            "Action selection is deterministic: argmin expected loss, no randomization",
            "The posterior sums to 1.0 after normalization (probability floor prevents zero-sum)",
        ],
        invalidation_signals: &[
            "E-process e-value exceeds alert threshold (20.0) → posterior calibration suspect",
            "CUSUM detects degradation → regime shift may invalidate loss matrix entries",
            "Conformal interval width exceeds decision margin → uncertainty too high for argmin",
            "Average regret exceeds 2× the initial baseline regret for >100 consecutive decisions",
        ],
        degradation_behavior: "Enters safe mode: high-force actions (ReleaseReservations) \
                               blocked, advisory-only mode until calibration recovers. \
                               Posterior floor (1e-10) prevents division by zero.",
        interaction_constraints: &[
            "atc.calibration.eprocess — calibration gate controls whether decisions are trusted",
            "atc.calibration.cusum — regime shifts trigger safe mode and loss matrix review",
            "atc.learning.pid_tuner — adjusts loss matrix entries based on regret signal",
        ],
    },
    // ── 2. Likelihood-Weighted EWMA Posterior ───────────────────────
    AssumptionEntry {
        method_id: "atc.core.ewma_posterior",
        name: "Likelihood-Weighted EWMA Posterior Updates",
        family: "Bayesian Updating",
        subsystems: &["liveness", "conflict", "load_routing"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Updates belief about agent/system state in O(|states|) per \
                         observation without requiring conjugate priors or MCMC, \
                         keeping the per-tick cost well within the 5ms budget.",
            failure_it_prevents: "Stale posteriors: if beliefs are not updated with \
                                  new evidence, the system acts on outdated information \
                                  (e.g., treating a recovered agent as still dead).",
            what_happens_without: "The system would need either a fixed-window empirical \
                                   estimate (loses responsiveness) or full Bayesian \
                                   inference (too expensive for 5ms tick budget).",
            promotion_evidence: "N/A — active by construction as the posterior update \
                                 mechanism for expected-loss decisions.",
            sunset_evidence: "If a conjugate-prior update with equivalent O(1) cost \
                              is implemented and achieves tighter conformal intervals \
                              over 5,000+ decisions, this EWMA approach may be replaced.",
            risk_tier: 1,
        },
        assumptions: &[
            "Likelihoods are non-negative (clamped at 0.0 if violated)",
            "Unspecified states receive likelihood 1.0 (uninformative prior for missing evidence)",
            "Alpha (learning rate) = 0.3 is appropriate for the observation cadence (~1 update per tick)",
            "The probability floor (1e-10) does not materially bias decisions in the 3-state space",
            "Observations arrive roughly uniformly over time (no bursty batches that overwhelm EWMA)",
        ],
        invalidation_signals: &[
            "Posterior concentrates on a single state with probability >0.999 for >50 consecutive ticks \
             (suggests floor is not saving a collapsed posterior)",
            "Alpha needs to be >0.5 for responsive updates, suggesting the EWMA half-life is too slow",
            "Observations arrive in bursts (>10 per tick) that overwhelm the exponential decay model",
        ],
        degradation_behavior: "Negative likelihoods are clamped to 0.0. Zero-sum posteriors \
                               trigger safe mode via the calibration guard. The floor (1e-10) \
                               prevents underflow but may bias very small probabilities.",
        interaction_constraints: &[
            "atc.core.expected_loss — consumes the posterior as input",
            "atc.calibration.eprocess — monitors whether the posterior is well-calibrated",
        ],
    },
    // ── 3. E-Process (Anytime-Valid Martingale) ─────────────────────
    AssumptionEntry {
        method_id: "atc.calibration.eprocess",
        name: "E-Process Miscalibration Detector",
        family: "Sequential Testing",
        subsystems: &["calibration"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Detects when ATC predictions are systematically wrong \
                         without requiring a predetermined sample size or multiple-testing \
                         correction, enabling immediate safe-mode entry.",
            failure_it_prevents: "Sustained miscalibration goes undetected: ATC keeps \
                                  making decisions based on a wrong posterior, leading to \
                                  a cascade of false-positive releases or missed deadlocks.",
            what_happens_without: "No automated miscalibration detection. Operators would \
                                   need to manually monitor accuracy, and sustained drift \
                                   would accumulate damage until a human intervenes.",
            promotion_evidence: "N/A — active by construction as the primary calibration \
                                 monitor.",
            sunset_evidence: "If a simpler running-accuracy check (e.g., windowed hit rate \
                              with fixed threshold) achieves equivalent detection latency \
                              with fewer false alarms over 10,000+ ticks in the synthetic \
                              corpus, the e-process may be replaced.",
            risk_tier: 2,
        },
        assumptions: &[
            "Predictions are exchangeable under the null hypothesis (correctly calibrated system)",
            "The binary correctness indicator (hit/miss) is well-defined for each decision",
            "Alert threshold (20.0) provides approximately 5% Type I error rate",
            "ONS adaptive bet sizing converges to near-optimal betting fractions",
            "Multiple per-subsystem and per-agent e-processes do not interact (independence)",
        ],
        invalidation_signals: &[
            "E-value oscillates rapidly between near-threshold and near-zero without converging",
            "Alert fires repeatedly with immediate recovery (suggests threshold is too sensitive for the noise level)",
            "Per-agent e-processes consistently disagree with the global e-process (heterogeneity violates exchangeability)",
        ],
        degradation_behavior: "E-value capped at 1e100 to prevent overflow. Safe mode entry \
                               is the primary response. drift_sources() provides per-subsystem \
                               and per-agent diagnostic output for targeted investigation.",
        interaction_constraints: &[
            "atc.calibration.cusum — both monitor calibration; either triggering enters safe mode",
            "atc.calibration.safe_mode — consumes e-process alert as entry condition",
            "atc.core.expected_loss — e-process gates whether decisions are trusted",
        ],
    },
    // ── 4. CUSUM Change-Point Detection ─────────────────────────────
    AssumptionEntry {
        method_id: "atc.calibration.cusum",
        name: "CUSUM Regime Shift Detector",
        family: "Sequential Analysis",
        subsystems: &["calibration"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Detects abrupt changes in ATC accuracy (both degradation and \
                         improvement) that the e-process may be slow to identify, enabling \
                         faster safe-mode entry and mode transitions.",
            failure_it_prevents: "A regime shift (e.g., new agent type joins with different \
                                  behavior patterns) causes sustained miscalibration that the \
                                  EWMA posterior cannot adapt to quickly enough.",
            what_happens_without: "Regime shifts are detected only by the e-process, which \
                                   may take O(1/delta²) observations to trigger. CUSUM \
                                   detects shifts in O(1/delta) observations.",
            promotion_evidence: "N/A — active by construction alongside the e-process.",
            sunset_evidence: "If BOCPD (Bayesian Online Change-Point Detection) is promoted \
                              to active status and provides equivalent detection latency with \
                              richer regime information, CUSUM may be retired.",
            risk_tier: 2,
        },
        assumptions: &[
            "Error rate is piecewise-constant (abrupt shifts, not gradual drift)",
            "The baseline error rate (0.15 = 1 - 0.85 coverage) is correctly specified",
            "Delta (0.1) is the minimum shift magnitude worth detecting",
            "CUSUM resets upon detection (a second shift requires re-accumulation)",
            "Bidirectional detection (degradation and improvement) uses independent accumulators",
        ],
        invalidation_signals: &[
            "CUSUM fires repeatedly within a short window (< 20 ticks) without stable regime establishment",
            "The detected shift magnitude is consistently < delta (CUSUM is detecting noise)",
            "Gradual drift causes both accumulators to grow slowly without either triggering — \
             a gap in coverage that the e-process must catch",
        ],
        degradation_behavior: "On detection: safe mode entry if degradation, mode upgrade if \
                               improvement. Regime change logged in history (max 50 entries). \
                               CUSUM accumulators reset to zero after each detection.",
        interaction_constraints: &[
            "atc.calibration.eprocess — complementary: CUSUM catches abrupt shifts, e-process catches sustained drift",
            "atc.calibration.safe_mode — CUSUM degradation is an entry condition",
            "atc.learning.pid_tuner — regime shifts may warrant loss matrix reset",
        ],
    },
    // ── 5. Conformal Prediction Intervals ───────────────────────────
    AssumptionEntry {
        method_id: "atc.calibration.conformal",
        name: "Conformal Prediction (Distribution-Free Coverage)",
        family: "Conformal Inference",
        subsystems: &["calibration", "decision_gating"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Provides finite-sample coverage guarantees for predicted loss \
                         bounds without any distributional assumptions, enabling the \
                         uncertainty-driven release gate that blocks releases when ATC \
                         is unsure.",
            failure_it_prevents: "ATC releases file reservations with high confidence but \
                                  the confidence is unjustified — the prediction interval \
                                  is too narrow, and the actual loss exceeds the bound.",
            what_happens_without: "No release gating. The WithholdRelease action (which \
                                   blocks releases when conformal uncertainty is high) \
                                   would not exist, and false-positive releases would \
                                   increase.",
            promotion_evidence: "N/A — active by construction as the uncertainty quantifier \
                                 for the release gate.",
            sunset_evidence: "If empirical coverage drops below 80% (nominal 90%) for \
                              >500 consecutive observations despite proper sliding window \
                              maintenance, the method is suspect.",
            risk_tier: 2,
        },
        assumptions: &[
            "Observations are exchangeable (the sliding window is representative of future observations)",
            "No distributional assumptions (distribution-free guarantee)",
            "Calibration window of 100 observations is sufficient for stable quantile estimates",
            "Minimum 30 observations required before producing intervals (enforced in code)",
            "The nonconformity score |predicted_loss - actual_loss| is a reasonable measure of surprise",
        ],
        invalidation_signals: &[
            "Empirical coverage (tracked via hits/predictions) drops below 80% over a 200-observation window",
            "Interval width grows monotonically without stabilizing (the prediction is getting worse, not better)",
            "The sliding window is stale: no new observations for >100 ticks (calibration data is outdated)",
        ],
        degradation_behavior: "Returns None if calibration window < 30 observations. \
                               Empirical coverage tracked via hits/predictions counter. \
                               is_uncertain() returns false if window too small (conservative: \
                               does not block releases when uncertain about uncertainty).",
        interaction_constraints: &[
            "atc.core.expected_loss — conformal intervals gate release decisions",
            "atc.calibration.safe_mode — conformal uncertainty contributes to safe mode assessment",
        ],
    },
    // ── 6. BOCPD (Bayesian Online Change-Point Detection) ───────────
    AssumptionEntry {
        method_id: "atc.regime.bocpd",
        name: "Bayesian Online Change-Point Detection",
        family: "Bayesian Changepoint",
        subsystems: &["regime_detection"],
        status: MethodStatus::Available,
        ev_gate: EvGate {
            user_value: "Would detect regime shifts in *both* mean and variance of agent \
                         behavior, providing richer diagnostic information than CUSUM \
                         (which only detects mean shifts).",
            failure_it_prevents: "Variance shifts (e.g., agent behavior becomes erratic \
                                  without changing its average interval) go undetected \
                                  by CUSUM.",
            what_happens_without: "CUSUM handles mean shifts adequately. Variance shifts \
                                   are caught indirectly by the e-process (increased error \
                                   rate) but with longer detection latency.",
            promotion_evidence: "A synthetic scenario where BOCPD detects a variance-only \
                                 regime shift >50 ticks faster than CUSUM+e-process combined, \
                                 AND the detection improves user-visible outcomes (fewer \
                                 false releases or missed deadlocks) by >5% relative.",
            sunset_evidence: "If BOCPD remains in Available status for >90 days without \
                              promotion evidence, it should be retired to reduce code \
                              maintenance burden.",
            risk_tier: 1,
        },
        assumptions: &[
            "Observations are Gaussian with unknown mean and variance (NIG conjugate prior)",
            "Change points follow an independent Poisson process with hazard rate 1/250",
            "Weakly informative prior: mu ~ N(0,1), sigma² ~ InvGamma(1,1)",
            "Run-length truncation at 300 does not discard material probability mass",
            "Startup suppression (first 15 observations) is sufficient to avoid false positives from prior",
        ],
        invalidation_signals: &[
            "Agent behavior distributions are clearly non-Gaussian (heavy tails, multimodal)",
            "Change points cluster (violates Poisson independence assumption)",
            "Run-length posterior concentrates at max truncation (300) consistently — loss of resolution",
        ],
        degradation_behavior: "Startup suppression avoids false positives for first 15 \
                               observations. NIG conjugate update is O(1) per observation. \
                               Log-gamma uses Lanczos approximation (9-coefficient series) \
                               for numerical stability. Log-sum-exp prevents overflow.",
        interaction_constraints: &[
            "atc.calibration.cusum — BOCPD would complement or replace CUSUM for regime detection",
            "atc.calibration.eprocess — BOCPD provides different evidence (run-length) than e-process",
        ],
    },
    // ── 7. Agent Rhythm Tracking ────────────────────────────────────
    AssumptionEntry {
        method_id: "atc.liveness.rhythm",
        name: "Agent Rhythm Tracking (EWMA + Gaussian Suspicion)",
        family: "Time Series",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Detects when an agent has been silent for too long relative to \
                         its own normal behavior pattern, triggering investigation (probes) \
                         before the agent's reservations cause blocking conflicts.",
            failure_it_prevents: "A crashed or frozen agent holds file reservations \
                                  indefinitely, blocking other agents from editing those \
                                  files. Without rhythm tracking, detection depends on \
                                  fixed timeouts that are too aggressive for slow agents \
                                  or too lenient for fast ones.",
            what_happens_without: "Fixed timeout (e.g., 300s for all agents). Claude Code \
                                   agents (typical 60s interval) would be detected slowly; \
                                   Codex agents (typical 120s interval) would trigger false \
                                   alarms frequently.",
            promotion_evidence: "N/A — active by construction as the primary liveness \
                                 detection mechanism.",
            sunset_evidence: "If all agent types converge to similar activity patterns \
                              (variance < 10% across program types), a simple fixed \
                              threshold would suffice.",
            risk_tier: 1,
        },
        assumptions: &[
            "Inter-activity intervals are approximately Gaussian (justified for the 3-sigma threshold)",
            "Agent activity patterns are stationary within the EWMA window (~10 observations)",
            "Program-type priors (claude-code=60s, codex=120s, etc.) are reasonable initial estimates",
            "Prior weight of 3.0 pseudo-observations provides adequate cold-start regularization",
            "k=3.0 Gaussian threshold yields ~0.3% false-positive rate under Gaussian assumption",
        ],
        invalidation_signals: &[
            "False-positive suspicion rate exceeds 2% (Gaussian assumption is too optimistic)",
            "Agent interval distributions are clearly bimodal (e.g., active bursts then long pauses)",
            "Program-type priors are consistently wrong for a new program type (prior weight too high)",
        ],
        degradation_behavior: "Uses effective_avg (prior-blended) to bias toward priors for \
                               first ~10 observations. Variance tracks deviation from running \
                               mean. Suspicion threshold adapts per-agent via the adaptive \
                               threshold (Beta-Binomial) mechanism.",
        interaction_constraints: &[
            "atc.liveness.adaptive_threshold — personalizes the k-sigma threshold per agent",
            "atc.liveness.population — provides program-type priors for cold start",
            "atc.liveness.probes — rhythm suspicion triggers probe scheduling",
        ],
    },
    // ── 8. SPRT Log-Likelihood Ratio ────────────────────────────────
    AssumptionEntry {
        method_id: "atc.liveness.sprt",
        name: "Sequential Probability Ratio Test (Log-LR Accumulator)",
        family: "Sequential Testing",
        subsystems: &["liveness"],
        status: MethodStatus::Placeholder,
        ev_gate: EvGate {
            user_value: "Would provide a principled Suspect→Dead transition test with \
                         bounded Type I and Type II error rates, avoiding both premature \
                         death declarations and indefinite suspicion states.",
            failure_it_prevents: "An agent is stuck in Suspect state indefinitely because \
                                  the evidence is ambiguous — neither clearly alive nor \
                                  clearly dead.",
            what_happens_without: "Death declaration relies on the rhythm EWMA + probe \
                                   results. The current approach works but does not provide \
                                   formal error bounds for the Suspect→Dead transition.",
            promotion_evidence: "A synthetic scenario demonstrating that SPRT resolves \
                                 ambiguous Suspect states >30% faster than the current \
                                 probe-count heuristic, without increasing false death \
                                 declarations.",
            sunset_evidence: "If the sprt_log_lr field is still unused after 90 days, \
                              remove the field to reduce struct size.",
            risk_tier: 1,
        },
        assumptions: &[
            "Evidence for alive vs. dead can be expressed as likelihood ratios",
            "Observations (probe responses) are independent under both hypotheses",
            "Type I (false death) and Type II (missed death) error rates are pre-specified",
        ],
        invalidation_signals: &[
            "Log-LR accumulates indefinitely without crossing either boundary (indeterminate evidence)",
        ],
        degradation_behavior: "Currently a placeholder: sprt_log_lr field exists but is \
                               not consumed in decision logic. No operational impact.",
        interaction_constraints: &[
            "atc.liveness.rhythm — provides the suspicion trigger that would feed into SPRT",
        ],
    },
    // ── 9. Hierarchical Bayesian Agent Population Model ─────────────
    AssumptionEntry {
        method_id: "atc.liveness.population",
        name: "Hierarchical Agent Population Model (Program-Type Cohorts)",
        family: "Hierarchical Bayesian",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Provides program-type-specific priors for new agents, so a new \
                         Claude Code agent starts with a 60s prior instead of the generic \
                         300s default, reducing false suspicion alerts during cold start.",
            failure_it_prevents: "New agents trigger immediate suspicion because the \
                                  default prior (300s) is too conservative for fast agents \
                                  (Claude Code at 60s) or too aggressive for slow agents.",
            what_happens_without: "All new agents use the same 300s default prior. Fast \
                                   agents get unnecessary probes for the first ~10 observations; \
                                   slow agents are falsely declared alive when they are actually \
                                   unresponsive.",
            promotion_evidence: "N/A — active by construction as the prior provider.",
            sunset_evidence: "If all agent programs converge to the same activity pattern \
                              (inter-program variance < 10% of intra-program variance), \
                              the hierarchical model adds no value over a single global prior.",
            risk_tier: 0,
        },
        assumptions: &[
            "Agent activity patterns cluster meaningfully by program type",
            "Within-program variance is smaller than between-program variance",
            "Welford's parallel merge accurately combines per-agent statistics into cohort stats",
            "5 program categories (claude-code, codex-cli, gemini-cli, copilot-cli, unknown) are sufficient",
            "Pseudo-count of 3 provides adequate prior strength without overwhelming early observations",
        ],
        invalidation_signals: &[
            "A new program type is introduced with behavior very different from all existing cohorts",
            "Intra-program variance exceeds between-program variance (cohorts are not informative)",
            "Welford merge produces negative variance (numerical instability in low-count cohorts)",
        ],
        degradation_behavior: "Falls back to 'unknown' cohort defaults (300s mean, 0.5×mean \
                               variance) if program type is not recognized. Lock poisoning \
                               on the population model silently uses defaults.",
        interaction_constraints: &[
            "atc.liveness.rhythm — consumes population priors for cold-start initialization",
        ],
    },
    // ── 10. Kaplan-Meier Survival Estimator ─────────────────────────
    AssumptionEntry {
        method_id: "atc.liveness.survival",
        name: "Kaplan-Meier Survival Estimator",
        family: "Non-Parametric Survival Analysis",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Models the probability that an agent 'survives' (remains active) \
                         as a function of silence duration, providing a hazard rate that \
                         informs liveness decisions more accurately than a fixed threshold.",
            failure_it_prevents: "A fixed silence threshold treats a 5-minute silence \
                                  identically for an agent that normally pauses 4 minutes \
                                  (barely suspicious) and one that normally pauses 30 seconds \
                                  (very suspicious). The survival curve captures this distinction.",
            what_happens_without: "Liveness decisions rely entirely on the rhythm EWMA and \
                                   Gaussian threshold, which assume normal distribution. The \
                                   survival curve handles heavy-tailed and skewed distributions.",
            promotion_evidence: "N/A — active by construction as an additional signal for \
                                 liveness posterior updates.",
            sunset_evidence: "If agent silence distributions are well-described by a \
                              Gaussian (Shapiro-Wilk p > 0.05 for 90% of agents), the \
                              survival estimator adds no value over the rhythm EWMA.",
            risk_tier: 0,
        },
        assumptions: &[
            "Censoring is independent of survival (agents that depart are not systematically dying)",
            "Observations are ordered by silence duration within the sliding window",
            "Window of 1000 observations is sufficient for stable survival estimates",
            "Hazard rate h(t) = (S(t) - S(t+window)) / (S(t) × window) is well-defined when S(t) > 0",
        ],
        invalidation_signals: &[
            "Censoring rate exceeds 50% (most observations are censored, not deaths — estimates are unreliable)",
            "S(t) is nearly flat across the observation range (no discriminating power)",
            "Hazard rate is consistently 1.0 (trivial: all observations are deaths, no survival information)",
        ],
        degradation_behavior: "Returns S(t)=1.0 if no observations. Returns h(t)=1.0 if \
                               S(t) ≤ 0. Both are conservative defaults that do not trigger \
                               false death declarations.",
        interaction_constraints: &[
            "atc.liveness.rhythm — survival curve complements the Gaussian suspicion threshold",
        ],
    },
    // ── 11. Adaptive Threshold (Beta-Binomial) ──────────────────────
    AssumptionEntry {
        method_id: "atc.liveness.adaptive_threshold",
        name: "Adaptive Suspicion Threshold (Beta-Binomial Posterior)",
        family: "Bayesian Updating",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Personalizes the suspicion threshold k for each agent based on \
                         observed true-positive and false-positive rates, so agents with \
                         erratic behavior get a wider threshold and stable agents get a \
                         tighter one.",
            failure_it_prevents: "A global k=3.0 threshold is too tight for erratic agents \
                                  (false positives) and too loose for stable agents (missed \
                                  true positives).",
            what_happens_without: "All agents use k=3.0. Erratic agents generate excessive \
                                   suspicion events; stable agents are detected late.",
            promotion_evidence: "N/A — active by construction as the threshold personalizer.",
            sunset_evidence: "If per-agent false-positive rates converge to <1% for all \
                              agents with the fixed k=3.0 threshold, the adaptive mechanism \
                              adds only complexity.",
            risk_tier: 0,
        },
        assumptions: &[
            "Per-agent precision (true-positive rate) is Beta-distributed",
            "Prior α=β=2 (weakly informative) is appropriate before any observations",
            "The effective_k formula (base_k + adjustment scaled by precision deviation) is monotonic",
            "k bounds [1.5, 5.0] prevent unreasonable threshold values",
            "Convergence requires ~25+ outcome observations (prior counts are subtracted for effective n)",
        ],
        invalidation_signals: &[
            "effective_k oscillates between min and max bounds (the Beta posterior is not converging)",
            "An agent's true-positive rate changes abruptly (regime shift in agent behavior)",
        ],
        degradation_behavior: "Clamps effective_k to [1.5, 5.0]. Prior pseudo-counts (4) \
                               are subtracted for observation count. New agents start at \
                               base_k=3.0 (the prior mean at α=β=2 gives precision=0.5).",
        interaction_constraints: &[
            "atc.liveness.rhythm — adapts the k-sigma threshold used by rhythm suspicion",
        ],
    },
    // ── 12. PID Controller for Loss Matrix Tuning ───────────────────
    AssumptionEntry {
        method_id: "atc.learning.pid_tuner",
        name: "PID Controller for Loss Matrix Adaptation",
        family: "Control Theory",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Automatically adjusts loss matrix entries based on observed regret, \
                         so the system learns from its mistakes without requiring manual \
                         operator tuning of cost parameters.",
            failure_it_prevents: "Loss matrix entries that were reasonable at deployment become \
                                  suboptimal as agent behavior patterns change, leading to \
                                  persistent suboptimal actions.",
            what_happens_without: "Loss matrices are fixed at their baseline values forever. \
                                   Operators must manually tune them when patterns change, \
                                   which requires deep understanding of the decision theory.",
            promotion_evidence: "N/A — active by construction as the learning mechanism.",
            sunset_evidence: "If cumulative regret with PID tuning is not statistically lower \
                              than cumulative regret with fixed loss matrices over 5,000+ \
                              decisions in a controlled experiment, the PID tuner should be \
                              disabled (it adds noise without reducing regret).",
            risk_tier: 1,
        },
        assumptions: &[
            "Regret is an unbiased signal of suboptimality for the associated (action, state) entry",
            "Loss matrix entries affect decisions approximately linearly (PID linearity assumption)",
            "k_p=0.1, k_i=0.01, k_d=0.02 gains are stable (no PID oscillation)",
            "Entry bounds [original×0.1, original×10.0] prevent runaway adaptation",
            "Anti-windup (integral clamp at original×2.0) prevents integral saturation",
        ],
        invalidation_signals: &[
            "A loss matrix entry oscillates between bounds (PID is unstable for this entry)",
            "Integral term saturates at the clamp value for >100 consecutive updates (persistent bias)",
            "Adapted loss matrix produces higher regret than the original fixed matrix",
        ],
        degradation_behavior: "Bounds clamp entries to [0.1×original, 10×original]. \
                               Anti-windup prevents integral saturation. Operator override \
                               resets to original value + zero integral. Entry changes are \
                               logged via transparency cards.",
        interaction_constraints: &[
            "atc.core.expected_loss — PID tuner modifies the loss matrices consumed by decision theory",
            "atc.tracking.regret — PID tuner consumes regret signal as its error input",
            "atc.calibration.cusum — regime shifts may warrant PID reset",
        ],
    },
    // ── 13. Regret Tracking ─────────────────────────────────────────
    AssumptionEntry {
        method_id: "atc.tracking.regret",
        name: "Regret Tracking (Counterfactual Loss Accounting)",
        family: "Online Learning",
        subsystems: &["liveness", "conflict", "load_routing"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Measures how much worse ATC's actions are compared to perfect \
                         hindsight, identifying which actions consistently underperform \
                         so the PID tuner can adjust.",
            failure_it_prevents: "Systematic suboptimality goes undetected: ATC makes \
                                  consistently bad decisions for one action type but the \
                                  average accuracy looks fine.",
            what_happens_without: "No per-action feedback signal. The PID tuner would have \
                                   nothing to optimize against. Loss matrices stay fixed.",
            promotion_evidence: "N/A — active by construction as the feedback signal for \
                                 loss matrix learning.",
            sunset_evidence: "If the learning stack is retired entirely, regret tracking \
                              remains useful as a diagnostic metric. It would only be \
                              retired if ATC itself is retired.",
            risk_tier: 0,
        },
        assumptions: &[
            "Best-in-hindsight loss is a meaningful counterfactual (true state is eventually known)",
            "Regret is non-negative (clamped at 0 if actual_loss < best_loss due to measurement noise)",
            "Window of 100 recent entries is sufficient for trend analysis",
            "Per-action cumulative regret is a fair comparison (actions may have different base costs)",
        ],
        invalidation_signals: &[
            "True state is never reliably determined (regret is computed against a guess, not ground truth)",
            "Regret signal is dominated by a single high-loss outlier (not representative of typical behavior)",
        ],
        degradation_behavior: "Clamps regret ≥ 0. Cumulative regret stored as f64 with no \
                               saturation guard (could overflow after ~10^308 decisions, \
                               which is not practically reachable).",
        interaction_constraints: &[
            "atc.learning.pid_tuner — PID consumes regret as its error signal",
            "atc.core.expected_loss — regret requires knowing the true state to compute hindsight loss",
        ],
    },
    // ── 14. VCG-Inspired Conflict Resolution Auction ────────────────
    AssumptionEntry {
        method_id: "atc.conflict.vcg",
        name: "VCG-Inspired Conflict Resolution Auction",
        family: "Mechanism Design",
        subsystems: &["conflict"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Resolves file reservation conflicts by prioritizing agents whose \
                         yielding would impose the highest cost on others, producing a \
                         fair ordering without randomization.",
            failure_it_prevents: "Arbitrary conflict resolution (e.g., alphabetical, FIFO) \
                                  that consistently disadvantages agents with more work at \
                                  stake.",
            what_happens_without: "Conflict resolution would fall back to simple heuristics \
                                   (first-come-first-served or random). Agents with high \
                                   externality (many tasks, near completion) could lose their \
                                   reservations to agents with minimal impact.",
            promotion_evidence: "N/A — active by construction as the conflict prioritization \
                                 mechanism.",
            sunset_evidence: "If file reservation conflicts become rare (<1 per day across \
                              all projects) because agents coordinate effectively via messaging, \
                              the VCG mechanism adds unnecessary complexity.",
            risk_tier: 1,
        },
        assumptions: &[
            "Task count and completion time are observable and truthfully reported (incentive compatibility)",
            "Externality computation (Σ tasks × time / 60 for all other agents) is a reasonable proxy for impact",
            "Conflict resolution is binary: one agent must yield (no partial sharing)",
            "Stable sort by name on tie ensures determinism",
        ],
        invalidation_signals: &[
            "Agents report inflated task counts to game the auction (incentive compatibility violated)",
            "Conflict resolution is consistently overridden by operators (the mechanism's ranking is not trusted)",
        ],
        degradation_behavior: "Returns empty list if participants list is empty. Stable sort \
                               by name on tie ensures deterministic output.",
        interaction_constraints: &[
            "atc.conflict.deadlock — deadlock detection may trigger conflict resolution",
        ],
    },
    // ── 15. Tarjan SCC (Deadlock Detection) ─────────────────────────
    AssumptionEntry {
        method_id: "atc.conflict.deadlock",
        name: "Tarjan SCC Deadlock Detection",
        family: "Graph Algorithm",
        subsystems: &["conflict"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Detects multi-agent deadlocks (circular reservation dependencies) \
                         that would otherwise require manual human intervention to resolve.",
            failure_it_prevents: "Two or more agents block each other's file reservations \
                                  in a cycle, causing all involved agents to stall \
                                  indefinitely.",
            what_happens_without: "Deadlocks are never detected automatically. Agents stall \
                                   until their reservation TTLs expire (potentially hours), \
                                   or an operator manually intervenes.",
            promotion_evidence: "N/A — active by construction as the deadlock detector.",
            sunset_evidence: "If the reservation system prevents deadlocks by design (e.g., \
                              global ordering on resources), Tarjan SCC is unnecessary.",
            risk_tier: 1,
        },
        assumptions: &[
            "The conflict graph accurately represents blocking relationships (edges = 'blocked by')",
            "SCCs with |V| > 1 are true deadlocks (not transient contention)",
            "The graph fits in memory (bounded by number of active agents × reservations)",
            "Graph edges are current (stale edges from expired reservations are removed)",
        ],
        invalidation_signals: &[
            "SCCs detected but agents resolve the conflict before intervention (false deadlock)",
            "Reservation edge TTL aging fails, causing stale edges to persist (phantom deadlocks)",
        ],
        degradation_behavior: "Returns empty list if graph has no edges or < 2 agents. \
                               Stack-based DFS is bounded by graph size. No false deadlocks \
                               are possible (SCC is exact for the given graph).",
        interaction_constraints: &[
            "atc.conflict.vcg — VCG is invoked to decide which agent yields in a detected deadlock",
        ],
    },
    // ── 16. Queueing Theory (Pollaczek-Khinchine + Kingman) ────────
    AssumptionEntry {
        method_id: "atc.load.queueing",
        name: "Queueing Theory (Pollaczek-Khinchine + Kingman Bounds)",
        family: "Queueing Theory",
        subsystems: &["load_routing"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Predicts queue congestion before it happens, enabling proactive \
                         load routing (Defer or SuggestAlternative) instead of reactive \
                         overload handling.",
            failure_it_prevents: "An agent's work queue becomes overloaded without warning, \
                                  causing cascading delays across dependent tasks.",
            what_happens_without: "Load routing relies on instantaneous queue depth only, \
                                   missing the trajectory (a queue at 50% utilization but \
                                   growing fast is more dangerous than one at 70% but stable).",
            promotion_evidence: "N/A — active by construction as the load prediction model.",
            sunset_evidence: "If agent workloads are always balanced (utilization variance < \
                              5% across agents), queueing predictions add no value over simple \
                              round-robin routing.",
            risk_tier: 0,
        },
        assumptions: &[
            "Queues are in equilibrium (utilization ρ < 1)",
            "Service times are i.i.d. within the observation window (M/G/1 assumption)",
            "Coefficient of variation accurately reflects service time variability",
            "Little's Law holds: L = λW (queue depth = arrival rate × sojourn time)",
            "Kingman G/G/1 bound is tight enough for routing decisions",
        ],
        invalidation_signals: &[
            "Utilization ρ ≥ 1.0 (queue is unstable — model returns infinity)",
            "Little's Law check fails: |L - λW| / L > tolerance",
            "Service time distribution is heavily multimodal (Cv² is misleading)",
        ],
        degradation_behavior: "Returns f64::INFINITY if ρ ≥ 1.0 (unstable queue). \
                               Kingman bound uses midpoint of arrival and service Cv² \
                               as a conservative estimate.",
        interaction_constraints: &[
            "atc.core.expected_loss — queueing predictions feed into load routing posteriors",
        ],
    },
    // ── 17. Information-Theoretic Probe Scheduling ──────────────────
    AssumptionEntry {
        method_id: "atc.liveness.probes",
        name: "Information-Theoretic Probe Scheduling (Entropy + Submodular)",
        family: "Information Theory / Submodular Optimization",
        subsystems: &["liveness"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Allocates the limited probe budget to agents where probing would \
                         reduce the most uncertainty, avoiding wasted probes on agents \
                         whose state is already well-known.",
            failure_it_prevents: "Probes are allocated uniformly or randomly, wasting budget \
                                  on agents with near-certain posteriors while neglecting \
                                  uncertain agents.",
            what_happens_without: "Round-robin probing. An agent with 99% posterior on Alive \
                                   gets probed as often as one with 50/50 Alive/Dead — \
                                   inefficient use of the probe budget.",
            promotion_evidence: "N/A — active by construction as the probe scheduler.",
            sunset_evidence: "If the probe budget is unlimited (no cost constraint), \
                              simple round-robin is equally effective and simpler.",
            risk_tier: 0,
        },
        assumptions: &[
            "Shannon entropy is a valid uncertainty measure for the 3-state discrete posterior",
            "Greedy submodular selection is near-optimal (1 - 1/e approximation ratio)",
            "Recency decay (60s half-life) captures diminishing returns from repeat probing",
            "min_gain threshold (0.001) effectively filters agents with near-certain posteriors",
            "Probing does not change the agent's true state (probes are non-invasive)",
        ],
        invalidation_signals: &[
            "Probed agents consistently do not change posterior (probes are uninformative — poor gain model)",
            "Entropy-ordered scheduling misses truly suspicious agents (entropy is high for wrong reasons)",
            "Recency decay is too aggressive: agents that genuinely need re-probing are filtered out",
        ],
        degradation_behavior: "Skips ATC's own agent and Dead agents (no useful information \
                               from probing these). Agents with gain ≤ 0.001 are filtered. \
                               Greedy selection stops when probe budget is exhausted.",
        interaction_constraints: &[
            "atc.liveness.rhythm — rhythm suspicion generates the posterior that entropy is computed over",
            "atc.core.ewma_posterior — probe results feed back into posterior updates",
            "atc.adaptive_mode — adaptive mode controller sets the probe budget",
        ],
    },
    // ── 18. Adaptive Mode Controller ────────────────────────────────
    AssumptionEntry {
        method_id: "atc.adaptive_mode",
        name: "Adaptive Mode Controller (Nominal / Pressure / Conservative)",
        family: "Resource Management",
        subsystems: &["global"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Gracefully reduces ATC overhead under system load by shedding \
                         non-critical work (probes, detailed analytics) while maintaining \
                         safety-critical functions (deadlock detection, release gating).",
            failure_it_prevents: "ATC consumes excessive CPU during system load spikes, \
                                  competing with the agents it is meant to serve.",
            what_happens_without: "ATC runs at full overhead regardless of system load. \
                                   Under heavy load, tick processing may exceed the 5ms \
                                   budget, causing missed ticks or delayed decisions.",
            promotion_evidence: "N/A — active by construction as the budget controller.",
            sunset_evidence: "If tick processing never exceeds 50% of the budget (2.5ms) \
                              even at maximum agent count, the adaptive mode adds complexity \
                              without benefit.",
            risk_tier: 1,
        },
        assumptions: &[
            "Utilization and debt ratio are accurate proxies for system load",
            "Mode transitions at 75% (Pressure) and 90% (Conservative) are appropriate thresholds",
            "Probe budget is the right knob to turn under load (probes are sheddable)",
            "Mode transitions are monotonic within a single direction (no chattering)",
        ],
        invalidation_signals: &[
            "Mode transitions chatter (rapid oscillation between Nominal and Pressure)",
            "Conservative mode is entered but tick processing time does not decrease (wrong bottleneck)",
            "Probe shedding causes missed liveness detections that would have been caught in Nominal mode",
        ],
        degradation_behavior: "Mode transitions are threshold-based with hysteresis (debt ratio \
                               provides the hysteresis). Downgrade on CUSUM degradation, upgrade \
                               on CUSUM improvement.",
        interaction_constraints: &[
            "atc.liveness.probes — adaptive mode sets the probe budget",
            "atc.calibration.cusum — regime shifts trigger mode transitions",
        ],
    },
    // ── 19. Calibration Guard (Safe Mode) ───────────────────────────
    AssumptionEntry {
        method_id: "atc.calibration.safe_mode",
        name: "Calibration Guard (Safe Mode Entry/Exit State Machine)",
        family: "Safety Monitor",
        subsystems: &["calibration"],
        status: MethodStatus::Active,
        ev_gate: EvGate {
            user_value: "Prevents cascading damage when ATC's calibration drifts by blocking \
                         high-force actions (reservation releases) until calibration recovers, \
                         ensuring that uncertain ATC never destroys agent work.",
            failure_it_prevents: "Miscalibrated ATC releases file reservations of active agents, \
                                  destroying their work in progress.",
            what_happens_without: "No safety circuit breaker. A miscalibrated ATC continues \
                                   making high-force decisions with incorrect posteriors, \
                                   potentially releasing multiple agents' reservations before \
                                   the problem is noticed.",
            promotion_evidence: "N/A — active by construction as the safety circuit breaker.",
            sunset_evidence: "Never. Safe mode is the last line of defense. It may be \
                              simplified but not removed.",
            risk_tier: 2,
        },
        assumptions: &[
            "E-process OR CUSUM triggering is sufficient evidence of miscalibration",
            "Safe mode recovery count (20 consecutive correct predictions) provides adequate confidence",
            "Blocking ReleaseReservations in safe mode is the right response (not blocking all actions)",
            "Probes continue in safe mode (information gathering is always safe)",
        ],
        invalidation_signals: &[
            "Safe mode is entered but calibration was actually fine (false alarm from e-process or CUSUM)",
            "Safe mode recovery count is too high: the system stays in safe mode unnecessarily long",
            "Safe mode does not block enough actions: harmful actions other than Release slip through",
        ],
        degradation_behavior: "Conservative: stays in safe mode until BOTH e-process AND CUSUM \
                               are healthy AND recovery count is met. In safe mode: Release \
                               blocked, Advisory may be blocked, Probes continue.",
        interaction_constraints: &[
            "atc.calibration.eprocess — entry condition",
            "atc.calibration.cusum — entry condition",
            "atc.core.expected_loss — safe mode gates which decisions are executed",
        ],
    },
];

// ──────────────────────────────────────────────────────────────────────
// Ledger query helpers
// ──────────────────────────────────────────────────────────────────────

/// Look up a method by its ID.
#[must_use]
pub fn find_method(method_id: &str) -> Option<&'static AssumptionEntry> {
    ASSUMPTIONS_LEDGER.iter().find(|e| e.method_id == method_id)
}

/// Return all methods with a given status.
#[must_use]
pub fn methods_by_status(status: MethodStatus) -> Vec<&'static AssumptionEntry> {
    ASSUMPTIONS_LEDGER
        .iter()
        .filter(|e| e.status == status)
        .collect()
}

/// Return all methods in a given subsystem.
#[must_use]
pub fn methods_in_subsystem(subsystem: &str) -> Vec<&'static AssumptionEntry> {
    ASSUMPTIONS_LEDGER
        .iter()
        .filter(|e| e.subsystems.contains(&subsystem))
        .collect()
}

/// Return all high-risk methods (`risk_tier` >= 2).
#[must_use]
pub fn high_risk_methods() -> Vec<&'static AssumptionEntry> {
    ASSUMPTIONS_LEDGER
        .iter()
        .filter(|e| e.ev_gate.risk_tier >= 2)
        .collect()
}

/// Validate that every method has complete fields.
///
/// Returns a list of validation errors. An empty list means all entries are valid.
#[must_use]
pub fn validate_ledger() -> Vec<String> {
    let mut errors = Vec::new();
    for entry in ASSUMPTIONS_LEDGER {
        if entry.ev_gate.user_value.is_empty() {
            errors.push(format!("{}: empty user_value in EV gate", entry.method_id));
        }
        if entry.ev_gate.failure_it_prevents.is_empty() {
            errors.push(format!(
                "{}: empty failure_it_prevents in EV gate",
                entry.method_id
            ));
        }
        if entry.ev_gate.what_happens_without.is_empty() {
            errors.push(format!(
                "{}: empty what_happens_without in EV gate",
                entry.method_id
            ));
        }
        if entry.assumptions.is_empty() {
            errors.push(format!("{}: no assumptions listed", entry.method_id));
        }
        if entry.invalidation_signals.is_empty() {
            errors.push(format!(
                "{}: no invalidation signals listed",
                entry.method_id
            ));
        }
        if entry.degradation_behavior.is_empty() {
            errors.push(format!("{}: empty degradation behavior", entry.method_id));
        }
    }
    errors
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_is_complete() {
        let errors = validate_ledger();
        assert!(
            errors.is_empty(),
            "Assumptions ledger has validation errors:\n{}",
            errors.join("\n")
        );
    }

    #[test]
    fn all_method_ids_unique() {
        let mut ids: Vec<&str> = ASSUMPTIONS_LEDGER.iter().map(|e| e.method_id).collect();
        ids.sort_unstable();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(len_before, ids.len(), "Duplicate method_id found in ledger");
    }

    #[test]
    fn all_entries_have_ev_gate() {
        for entry in ASSUMPTIONS_LEDGER {
            assert!(
                !entry.ev_gate.user_value.is_empty(),
                "{} has empty EV gate user_value",
                entry.method_id
            );
            assert!(
                !entry.ev_gate.failure_it_prevents.is_empty(),
                "{} has empty EV gate failure_it_prevents",
                entry.method_id
            );
        }
    }

    #[test]
    fn high_risk_methods_have_strong_degradation() {
        for entry in high_risk_methods() {
            assert!(
                entry.degradation_behavior.len() > 50,
                "{} is high-risk but has a short degradation description ({})",
                entry.method_id,
                entry.degradation_behavior.len()
            );
        }
    }

    #[test]
    fn interaction_constraints_reference_valid_ids() {
        let all_ids: Vec<&str> = ASSUMPTIONS_LEDGER.iter().map(|e| e.method_id).collect();
        for entry in ASSUMPTIONS_LEDGER {
            for constraint in entry.interaction_constraints {
                // Extract method_id from constraint (format: "method_id — description")
                let referenced_id = constraint.split(" — ").next().unwrap_or(constraint);
                assert!(
                    all_ids.contains(&referenced_id),
                    "{} references unknown method '{}' in interaction_constraints",
                    entry.method_id,
                    referenced_id
                );
            }
        }
    }

    #[test]
    fn active_methods_have_invalidation_signals() {
        for entry in ASSUMPTIONS_LEDGER {
            if entry.status == MethodStatus::Active {
                assert!(
                    !entry.invalidation_signals.is_empty(),
                    "Active method {} has no invalidation signals",
                    entry.method_id
                );
            }
        }
    }

    #[test]
    fn escalation_criteria_non_empty() {
        assert!(!ESCALATION_CRITERIA.is_empty());
        assert!(!SIMPLIFICATION_CRITERIA.is_empty());
    }

    #[test]
    fn transparency_card_serde_roundtrip() {
        let card = TransparencyCard {
            card_id: 1,
            timestamp_iso: "2026-03-18T00:00:00Z".to_string(),
            method_id: "atc.learning.pid_tuner".to_string(),
            policy_id: "liveness.loss_matrix.release_alive".to_string(),
            evidence_id: "evi-42".to_string(),
            regime_context: RegimeContext {
                adaptive_mode: "Nominal".to_string(),
                safe_mode_active: false,
                eprocess_value: 3.5,
                cusum_s_pos: 1.2,
                cusum_s_neg: 0.4,
                active_agent_count: 5,
                tick_number: 10_000,
            },
            change_description: "loss[Release][Alive] adjusted from 100.0 to 95.0".to_string(),
            rationale: "Average regret for Release action exceeded 2.0 over last 50 decisions, \
                        PID integral accumulated to 4.8"
                .to_string(),
            expected_loss_before: 12.5,
            expected_loss_after: 11.8,
            trigger: CardTrigger::Automatic,
            evidence_experience_ids: vec!["exp-100".to_string(), "exp-101".to_string()],
            reverses_card_id: None,
        };
        let json = serde_json::to_string(&card).unwrap();
        let decoded: TransparencyCard = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.card_id, 1);
        assert_eq!(decoded.method_id, "atc.learning.pid_tuner");
    }

    #[test]
    fn find_method_works() {
        assert!(find_method("atc.core.expected_loss").is_some());
        assert!(find_method("atc.nonexistent").is_none());
    }

    #[test]
    fn methods_by_subsystem_works() {
        let liveness = methods_in_subsystem("liveness");
        assert!(
            liveness.len() >= 5,
            "Expected at least 5 liveness methods, got {}",
            liveness.len()
        );
    }

    #[test]
    fn methods_by_status_works() {
        let active = methods_by_status(MethodStatus::Active);
        assert!(
            active.len() >= 10,
            "Expected at least 10 active methods, got {}",
            active.len()
        );

        let placeholders = methods_by_status(MethodStatus::Placeholder);
        assert_eq!(
            placeholders.len(),
            1,
            "Expected exactly 1 placeholder (SPRT)"
        );
    }
}
