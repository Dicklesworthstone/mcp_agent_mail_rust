//! Regime detection, hysteresis, and history discounting for ATC learning
//! (br-0qt6e.3.6).
//!
//! Defines the nonstationarity contract for the ATC learning stack: when
//! the environment has changed enough that old evidence should count less,
//! how to transition between regimes with stability guarantees, and what
//! gets reset, discounted, or frozen.
//!
//! # Regime Lifecycle
//!
//! ```text
//!  ┌──────────┐     CUSUM/BOCPD      ┌──────────────┐   dwell elapsed   ┌────────┐
//!  │  Stable  │ ──────────────────── │ Transitioning │ ───────────────── │ Stable │
//!  └──────────┘     detection        └──────┬───────┘   (min_dwell_secs)└────────┘
//!                                           │
//!                                    rapid re-detection
//!                                           │
//!                                    ┌──────▼───────┐   timeout          ┌────────┐
//!                                    │   Cooling    │ ───────────────── │ Stable │
//!                                    └──────────────┘  (2× min_dwell)   └────────┘
//! ```
//!
//! # History Discounting on Regime Change
//!
//! When a regime change is confirmed (transition completes dwell period):
//! - EWMA rollup weights are halved (discount factor = 0.5)
//! - Conformal calibration windows are flushed
//! - Policy promotion is gated until the new regime is stable
//! - Old evidence is NOT deleted — it remains for audit — but its
//!   contribution to learning is diminished
//!
//! # Hysteresis
//!
//! The minimum dwell time prevents flapping: after a regime change is
//! detected, the system must stay in the new regime for at least
//! `min_dwell_secs` before another transition is allowed. During the
//! dwell period, further CUSUM detections are logged but suppressed.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

/// Default minimum dwell time between regime transitions (seconds).
pub const DEFAULT_MIN_DWELL_SECS: u64 = 300; // 5 minutes

/// Default EWMA discount factor applied on regime change.
pub const DEFAULT_REGIME_DISCOUNT: f64 = 0.5;

/// Default cooling period multiplier (applied on rapid re-detection).
pub const DEFAULT_COOLING_MULTIPLIER: u64 = 2;

/// Regime stability state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegimePhase {
    /// Operating in a stable regime. Learning proceeds normally.
    Stable,
    /// A regime change was detected but the minimum dwell period has
    /// not elapsed. Policy promotion is gated. History discounting
    /// has been applied.
    Transitioning,
    /// Rapid successive detections triggered a cooling period.
    /// All adaptation is frozen until the cooling period elapses.
    Cooling,
}

impl std::fmt::Display for RegimePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stable => write!(f, "stable"),
            Self::Transitioning => write!(f, "transitioning"),
            Self::Cooling => write!(f, "cooling"),
        }
    }
}

/// A regime identifier.
///
/// Regime IDs are monotonically increasing integers that uniquely
/// identify each stable period. The ID increments when a transition
/// completes its dwell period and the system returns to `Stable`.
pub type RegimeId = u64;

/// Regime state machine.
///
/// Tracks the current regime, transition state, and hysteresis counters.
/// This struct is designed to be embedded in the operator loop context
/// (not in the ATC engine, which handles detection but not management).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeManager {
    /// Current regime ID.
    pub current_regime_id: RegimeId,
    /// Current phase.
    pub phase: RegimePhase,
    /// When the current phase started (microseconds since epoch).
    pub phase_started_ts_micros: i64,
    /// Minimum dwell time between transitions (seconds).
    pub min_dwell_secs: u64,
    /// EWMA discount factor applied on regime change.
    pub discount_factor: f64,
    /// How many consecutive detections without stable dwell.
    pub rapid_detection_count: u32,
    /// Maximum rapid detections before entering Cooling.
    pub max_rapid_detections: u32,
    /// Whether policy promotion is currently gated.
    pub promotion_gated: bool,
    /// Whether history discounting was applied for the current transition.
    pub discount_applied: bool,
    /// Total regime changes since startup.
    pub total_transitions: u64,
    /// The reason for the most recent transition.
    pub last_transition_reason: Option<String>,
}

/// Result of processing a regime detection signal.
#[derive(Debug, Clone)]
pub enum RegimeAction {
    /// No action needed — stable or within dwell.
    None,
    /// A regime change has been confirmed after dwell.
    /// The new regime ID and transition reason are provided.
    TransitionConfirmed {
        new_regime_id: RegimeId,
        reason: String,
    },
    /// Detection was suppressed due to hysteresis (within dwell).
    Suppressed {
        reason: &'static str,
    },
    /// System entered cooling due to rapid detections.
    CoolingEntered {
        rapid_count: u32,
    },
    /// Discount should be applied to EWMA rollups.
    DiscountRollups {
        factor: f64,
    },
}

impl RegimeManager {
    /// Create a new regime manager with default settings.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_regime_id: 0,
            phase: RegimePhase::Stable,
            phase_started_ts_micros: 0,
            min_dwell_secs: DEFAULT_MIN_DWELL_SECS,
            discount_factor: DEFAULT_REGIME_DISCOUNT,
            rapid_detection_count: 0,
            max_rapid_detections: 3,
            promotion_gated: false,
            discount_applied: false,
            total_transitions: 0,
            last_transition_reason: None,
        }
    }

    /// Process a CUSUM/BOCPD detection signal.
    ///
    /// Returns the action the caller should take (if any).
    pub fn on_detection(
        &mut self,
        now_micros: i64,
        direction: &str,
        cusum_value: f64,
    ) -> RegimeAction {
        match self.phase {
            RegimePhase::Stable => {
                // Enter transitioning.
                self.phase = RegimePhase::Transitioning;
                self.phase_started_ts_micros = now_micros;
                self.promotion_gated = true;
                self.last_transition_reason = Some(format!(
                    "{direction} detected (CUSUM={cusum_value:.3})"
                ));

                // Apply discount immediately on entering transition.
                self.discount_applied = true;
                RegimeAction::DiscountRollups {
                    factor: self.discount_factor,
                }
            }
            RegimePhase::Transitioning => {
                // Another detection while already transitioning.
                self.rapid_detection_count += 1;

                if self.rapid_detection_count >= self.max_rapid_detections {
                    // Too many rapid detections — enter cooling.
                    self.phase = RegimePhase::Cooling;
                    self.phase_started_ts_micros = now_micros;
                    RegimeAction::CoolingEntered {
                        rapid_count: self.rapid_detection_count,
                    }
                } else {
                    RegimeAction::Suppressed {
                        reason: "within dwell period (hysteresis)",
                    }
                }
            }
            RegimePhase::Cooling => {
                // Suppress all detections during cooling.
                RegimeAction::Suppressed {
                    reason: "cooling period active",
                }
            }
        }
    }

    /// Check if the current transition is complete (dwell period elapsed).
    ///
    /// Call this periodically (e.g., every tick) to advance the regime
    /// state machine.
    pub fn tick(&mut self, now_micros: i64) -> RegimeAction {
        let dwell_micros = i64::try_from(self.min_dwell_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000);

        match self.phase {
            RegimePhase::Transitioning => {
                let elapsed = now_micros.saturating_sub(self.phase_started_ts_micros);
                if elapsed >= dwell_micros {
                    // Dwell complete — confirm transition.
                    self.current_regime_id += 1;
                    self.total_transitions += 1;
                    self.phase = RegimePhase::Stable;
                    self.phase_started_ts_micros = now_micros;
                    self.promotion_gated = false;
                    self.rapid_detection_count = 0;
                    self.discount_applied = false; // reset so next transition re-applies

                    let reason = self
                        .last_transition_reason
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());

                    RegimeAction::TransitionConfirmed {
                        new_regime_id: self.current_regime_id,
                        reason,
                    }
                } else {
                    RegimeAction::None
                }
            }
            RegimePhase::Cooling => {
                let cooling_micros = dwell_micros.saturating_mul(2);
                let elapsed = now_micros.saturating_sub(self.phase_started_ts_micros);
                if elapsed >= cooling_micros {
                    // Cooling complete — return to stable with new regime.
                    self.current_regime_id += 1;
                    self.total_transitions += 1;
                    self.phase = RegimePhase::Stable;
                    self.phase_started_ts_micros = now_micros;
                    self.promotion_gated = false;
                    self.rapid_detection_count = 0;
                    self.discount_applied = false; // reset for next transition

                    RegimeAction::TransitionConfirmed {
                        new_regime_id: self.current_regime_id,
                        reason: "cooling period elapsed".to_string(),
                    }
                } else {
                    RegimeAction::None
                }
            }
            RegimePhase::Stable => RegimeAction::None,
        }
    }

    /// Whether policy promotion is currently allowed.
    #[must_use]
    pub const fn promotion_allowed(&self) -> bool {
        !self.promotion_gated
    }

    /// Whether the system is in a stable regime.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        self.phase == RegimePhase::Stable
    }

    /// Get a compact summary for operator display.
    #[must_use]
    pub fn summary(&self, now_micros: i64) -> RegimeSummary {
        let phase_age_secs =
            now_micros.saturating_sub(self.phase_started_ts_micros) / 1_000_000;

        RegimeSummary {
            regime_id: self.current_regime_id,
            phase: self.phase,
            phase_age_secs: u64::try_from(phase_age_secs).unwrap_or(0),
            promotion_gated: self.promotion_gated,
            total_transitions: self.total_transitions,
            last_reason: self.last_transition_reason.clone(),
        }
    }
}

impl Default for RegimeManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Compact regime summary for operator surfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeSummary {
    pub regime_id: RegimeId,
    pub phase: RegimePhase,
    pub phase_age_secs: u64,
    pub promotion_gated: bool,
    pub total_transitions: u64,
    pub last_reason: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_stable() {
        let mgr = RegimeManager::new();
        assert!(mgr.is_stable());
        assert!(mgr.promotion_allowed());
        assert_eq!(mgr.current_regime_id, 0);
    }

    #[test]
    fn detection_triggers_transition() {
        let mut mgr = RegimeManager::new();
        let action = mgr.on_detection(1_000_000, "degradation", 5.5);
        assert!(matches!(action, RegimeAction::DiscountRollups { .. }));
        assert_eq!(mgr.phase, RegimePhase::Transitioning);
        assert!(!mgr.promotion_allowed());
    }

    #[test]
    fn dwell_completes_transition() {
        let mut mgr = RegimeManager::new();
        mgr.min_dwell_secs = 1; // 1 second for testing

        mgr.on_detection(1_000_000, "degradation", 5.5);
        assert_eq!(mgr.phase, RegimePhase::Transitioning);

        // Before dwell completes.
        let action = mgr.tick(1_500_000);
        assert!(matches!(action, RegimeAction::None));

        // After dwell completes (1 second = 1_000_000 micros).
        let action = mgr.tick(2_100_000);
        assert!(matches!(
            action,
            RegimeAction::TransitionConfirmed { new_regime_id: 1, .. }
        ));
        assert!(mgr.is_stable());
        assert!(mgr.promotion_allowed());
        assert_eq!(mgr.current_regime_id, 1);
    }

    #[test]
    fn hysteresis_suppresses_during_dwell() {
        let mut mgr = RegimeManager::new();
        mgr.min_dwell_secs = 10;

        mgr.on_detection(1_000_000, "degradation", 5.5);
        let action = mgr.on_detection(2_000_000, "improvement", 3.0);
        assert!(matches!(action, RegimeAction::Suppressed { .. }));
    }

    #[test]
    fn rapid_detections_trigger_cooling() {
        let mut mgr = RegimeManager::new();
        mgr.max_rapid_detections = 2;
        mgr.min_dwell_secs = 10;

        mgr.on_detection(1_000_000, "degradation", 5.5);
        mgr.on_detection(2_000_000, "improvement", 3.0); // rapid_count = 1
        let action = mgr.on_detection(3_000_000, "degradation", 6.0); // rapid_count = 2
        assert!(matches!(action, RegimeAction::CoolingEntered { .. }));
        assert_eq!(mgr.phase, RegimePhase::Cooling);
    }

    #[test]
    fn cooling_suppresses_all_detections() {
        let mut mgr = RegimeManager::new();
        mgr.max_rapid_detections = 1;
        mgr.min_dwell_secs = 10;

        mgr.on_detection(1_000_000, "degradation", 5.5);
        mgr.on_detection(2_000_000, "improvement", 3.0); // enters cooling
        let action = mgr.on_detection(3_000_000, "degradation", 7.0);
        assert!(matches!(action, RegimeAction::Suppressed { .. }));
    }

    #[test]
    fn cooling_resolves_to_stable() {
        let mut mgr = RegimeManager::new();
        mgr.max_rapid_detections = 1;
        mgr.min_dwell_secs = 1; // 1 second

        mgr.on_detection(1_000_000, "degradation", 5.5);
        mgr.on_detection(2_000_000, "improvement", 3.0); // enters cooling

        // Cooling period = 2 × dwell = 2 seconds = 2_000_000 micros
        let action = mgr.tick(4_100_000); // 2.1s after cooling start
        assert!(matches!(
            action,
            RegimeAction::TransitionConfirmed { new_regime_id: 1, .. }
        ));
        assert!(mgr.is_stable());
    }

    #[test]
    fn summary_shows_correct_state() {
        let mut mgr = RegimeManager::new();
        let summary = mgr.summary(1_000_000);
        assert_eq!(summary.regime_id, 0);
        assert_eq!(summary.phase, RegimePhase::Stable);
        assert!(!summary.promotion_gated);

        mgr.on_detection(1_000_000, "degradation", 5.5);
        let summary = mgr.summary(2_000_000);
        assert_eq!(summary.phase, RegimePhase::Transitioning);
        assert!(summary.promotion_gated);
    }

    #[test]
    fn multiple_regime_transitions_increment_id() {
        let mut mgr = RegimeManager::new();
        mgr.min_dwell_secs = 0; // instant dwell for testing

        mgr.on_detection(1_000_000, "degradation", 5.5);
        mgr.tick(1_000_001); // completes instantly
        assert_eq!(mgr.current_regime_id, 1);

        mgr.on_detection(2_000_000, "improvement", 3.0);
        mgr.tick(2_000_001);
        assert_eq!(mgr.current_regime_id, 2);
        assert_eq!(mgr.total_transitions, 2);
    }
}
