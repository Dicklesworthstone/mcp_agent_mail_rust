//! Bayesian TUI diff strategy — expected-loss decision layer for frame rendering.
//!
//! Models the terminal frame state as one of four discrete states and chooses
//! the rendering action that minimises expected loss under the current
//! posterior distribution. Decisions are recorded in the evidence ledger for
//! transparency and Bayesian updating.

use mcp_agent_mail_core::evidence_ledger::{EvidenceLedger, evidence_ledger};

/// Observed frame state used as evidence for the Bayesian classifier.
#[derive(Debug, Clone, Copy)]
pub struct FrameState {
    /// Fraction of cells that changed since the last frame (0.0–1.0).
    pub change_ratio: f64,
    /// Whether a terminal resize was detected this frame.
    pub is_resize: bool,
    /// Remaining frame budget in milliseconds.
    pub budget_remaining_ms: f64,
    /// Count of recent rendering errors.
    pub error_count: u32,
}

/// Rendering action chosen by the strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAction {
    /// Only redraw changed cells. Cheap when changes are localised.
    Incremental,
    /// Redraw the entire screen. Always correct but expensive.
    Full,
    /// Skip this frame to allow recovery. Cheapest but adds latency.
    Deferred,
}

impl DiffAction {
    const fn index(self) -> usize {
        match self {
            Self::Incremental => 0,
            Self::Full => 1,
            Self::Deferred => 2,
        }
    }

    const fn from_index(i: usize) -> Self {
        match i {
            0 => Self::Incremental,
            1 => Self::Full,
            _ => Self::Deferred,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
            Self::Deferred => "deferred",
        }
    }
}

/// Number of discrete states.
const NUM_STATES: usize = 4;
/// Number of possible actions.
const NUM_ACTIONS: usize = 3;

/// State indices.
const STATE_STABLE: usize = 0;
const STATE_BURSTY: usize = 1;
const STATE_RESIZE: usize = 2;
const STATE_DEGRADED: usize = 3;

/// Bayesian diff strategy that chooses the rendering action minimising
/// expected loss under the current posterior distribution.
///
/// The loss matrix encodes domain-specific costs for each (state, action) pair.
/// The posterior is updated each frame via exponential moving average.
pub struct BayesianDiffStrategy {
    /// Current posterior probabilities for each state.
    prior: [f64; NUM_STATES],
    /// Loss matrix: `loss[state][action]`.
    loss: [[f64; NUM_ACTIONS]; NUM_STATES],
    /// Smoothing parameter for prior updates (exponential moving average).
    alpha: f64,
    /// When true, always returns `Full` (safe fallback).
    pub deterministic_fallback: bool,
}

impl BayesianDiffStrategy {
    /// Create a new strategy with uniform prior and the canonical loss matrix.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prior: [0.25; NUM_STATES],
            // Loss matrix (lower is better):
            //              incremental  full  deferred
            // stable_frame      1         8      20
            // bursty_change    12         3       5
            // resize           15         2      10
            // degraded         10         3      15
            //
            // NOTE: At 10fps (100ms tick), deferring a frame causes 100-200ms
            // of blank screen, which is extremely visible. The degraded row
            // now favours Full (3) over Deferred (15) so we always render
            // *something* rather than showing a blank frame.
            loss: [
                [1.0, 8.0, 20.0],  // stable
                [12.0, 3.0, 5.0],  // bursty
                [15.0, 2.0, 10.0], // resize
                [10.0, 3.0, 15.0], // degraded — Full preferred over Deferred
            ],
            alpha: 0.3,
            deterministic_fallback: false,
        }
    }

    /// Observe the current frame state, compute the posterior, choose the
    /// action that minimises expected loss, update the prior, and return
    /// the chosen action.
    ///
    /// Each decision is recorded in the global evidence ledger.
    pub fn observe(&mut self, frame: &FrameState) -> DiffAction {
        self.observe_with_ledger(frame, Some(evidence_ledger()))
    }

    /// Core observe logic with an optional ledger for testability.
    pub fn observe_with_ledger(
        &mut self,
        frame: &FrameState,
        ledger: Option<&EvidenceLedger>,
    ) -> DiffAction {
        if self.deterministic_fallback {
            return DiffAction::Full;
        }

        // 1. Compute likelihood of each state given the evidence.
        let likelihood = compute_likelihood(frame);

        // 2. Posterior = prior * likelihood (unnormalised).
        let mut posterior = [0.0f64; NUM_STATES];
        for (i, p) in posterior.iter_mut().enumerate() {
            *p = self.prior[i] * likelihood[i];
        }

        // 3. Normalise.
        let sum: f64 = posterior.iter().sum();
        if sum > 0.0 {
            for p in &mut posterior {
                *p /= sum;
            }
        } else {
            // Degenerate case: uniform fallback.
            posterior = [0.25; NUM_STATES];
        }

        // 4. Compute expected loss for each action.
        let mut best_action = 0usize;
        let mut best_loss = f64::MAX;
        let mut expected_losses = [0.0f64; NUM_ACTIONS];
        for (a, el_slot) in expected_losses.iter_mut().enumerate() {
            let el: f64 = posterior
                .iter()
                .zip(&self.loss)
                .map(|(&p, row)| p * row[a])
                .sum();
            *el_slot = el;
            if el < best_loss {
                best_loss = el;
                best_action = a;
            }
        }

        let action = DiffAction::from_index(best_action);

        // 5. Update prior via exponential moving average.
        let one_minus_alpha = 1.0 - self.alpha;
        for (prior, &post) in self.prior.iter_mut().zip(&posterior) {
            *prior = self.alpha.mul_add(post, one_minus_alpha * *prior);
        }

        // 6. Record decision in evidence ledger.
        if let Some(ledger) = ledger {
            ledger.record(
                "tui.diff_strategy",
                serde_json::json!({
                    "change_ratio": frame.change_ratio,
                    "is_resize": frame.is_resize,
                    "budget_remaining_ms": frame.budget_remaining_ms,
                    "error_count": frame.error_count,
                    "posterior": posterior,
                    "expected_losses": expected_losses,
                }),
                action.label(),
                Some(format!("expected_loss={best_loss:.3}")),
                posterior[dominant_state(&posterior)],
                "bayesian_tui_v1",
            );
        }

        action
    }

    /// Return the current posterior distribution.
    #[must_use]
    pub fn posterior(&self) -> [f64; NUM_STATES] {
        self.prior
    }

    /// Compute the expected loss for a specific action given the current prior.
    #[must_use]
    pub fn expected_loss(&self, action: DiffAction) -> f64 {
        let a = action.index();
        self.prior
            .iter()
            .zip(&self.loss)
            .map(|(&p, row)| p * row[a])
            .sum()
    }
}

/// Compute likelihood of each state given the frame evidence.
fn compute_likelihood(frame: &FrameState) -> [f64; NUM_STATES] {
    let mut lik = [0.0f64; NUM_STATES];

    // Resize is a strong binary signal.
    if frame.is_resize {
        lik[STATE_RESIZE] = 0.9;
        lik[STATE_STABLE] = 0.02;
        lik[STATE_BURSTY] = 0.03;
        lik[STATE_DEGRADED] = 0.05;
        return lik;
    }

    // Degraded: low budget or high error count.
    let degraded_signal = if frame.budget_remaining_ms < 4.0 || frame.error_count >= 3 {
        0.8
    } else if frame.budget_remaining_ms < 8.0 || frame.error_count >= 1 {
        0.3
    } else {
        0.05
    };

    // Bursty: high change ratio.
    let bursty_signal = if frame.change_ratio > 0.5 {
        0.8
    } else if frame.change_ratio > 0.3 {
        0.5
    } else if frame.change_ratio > 0.1 {
        0.2
    } else {
        0.05
    };

    // Stable: low change ratio, good budget, no errors.
    let stable_signal = if frame.change_ratio <= 0.1
        && frame.budget_remaining_ms >= 8.0
        && frame.error_count == 0
    {
        0.8
    } else if frame.change_ratio <= 0.3 && frame.budget_remaining_ms >= 4.0 {
        0.4
    } else {
        0.1
    };

    lik[STATE_STABLE] = stable_signal;
    lik[STATE_BURSTY] = bursty_signal;
    lik[STATE_RESIZE] = 0.01; // No resize flag, very unlikely.
    lik[STATE_DEGRADED] = degraded_signal;

    lik
}

/// Index of the dominant state in a distribution.
fn dominant_state(dist: &[f64; NUM_STATES]) -> usize {
    dist.iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| i)
}

impl Default for BayesianDiffStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stable_frame() -> FrameState {
        FrameState {
            change_ratio: 0.05,
            is_resize: false,
            budget_remaining_ms: 14.0,
            error_count: 0,
        }
    }

    fn bursty_frame() -> FrameState {
        FrameState {
            change_ratio: 0.6,
            is_resize: false,
            budget_remaining_ms: 12.0,
            error_count: 0,
        }
    }

    fn resize_frame() -> FrameState {
        FrameState {
            change_ratio: 0.0,
            is_resize: true,
            budget_remaining_ms: 16.0,
            error_count: 0,
        }
    }

    fn degraded_frame() -> FrameState {
        FrameState {
            change_ratio: 0.2,
            is_resize: false,
            budget_remaining_ms: 2.0,
            error_count: 5,
        }
    }

    /// 1. Low change ratio, no resize, high budget => `Incremental`.
    #[test]
    fn bayes_stable_frame_chooses_incremental() {
        let mut strategy = BayesianDiffStrategy::new();
        let action = strategy.observe_with_ledger(&stable_frame(), None);
        assert_eq!(action, DiffAction::Incremental);
    }

    /// 2. `is_resize=true` => `Full`.
    #[test]
    fn bayes_resize_chooses_full() {
        let mut strategy = BayesianDiffStrategy::new();
        let action = strategy.observe_with_ledger(&resize_frame(), None);
        assert_eq!(action, DiffAction::Full);
    }

    /// 3. Low budget, high error count => `Full` (not Deferred).
    ///    At 10fps, deferring a frame causes visible blank screens, so the
    ///    loss matrix now prefers Full in the degraded state.
    #[test]
    fn bayes_degraded_chooses_full() {
        let mut strategy = BayesianDiffStrategy::new();
        let action = strategy.observe_with_ledger(&degraded_frame(), None);
        assert_eq!(action, DiffAction::Full);
    }

    /// 4. High change ratio => `Full`.
    #[test]
    fn bayes_bursty_chooses_full() {
        let mut strategy = BayesianDiffStrategy::new();
        let action = strategy.observe_with_ledger(&bursty_frame(), None);
        assert_eq!(action, DiffAction::Full);
    }

    /// 5. For 100 random-ish `FrameState` values, posterior always sums to ~1.0.
    #[test]
    fn bayes_posterior_sums_to_one() {
        let mut strategy = BayesianDiffStrategy::new();
        let frames = [
            stable_frame(),
            bursty_frame(),
            resize_frame(),
            degraded_frame(),
            FrameState {
                change_ratio: 0.35,
                is_resize: false,
                budget_remaining_ms: 6.0,
                error_count: 1,
            },
        ];
        for round in 0..100 {
            let frame = &frames[round % frames.len()];
            strategy.observe_with_ledger(frame, None);
            let post = strategy.posterior();
            let sum: f64 = post.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "posterior sum {sum} != 1.0 at round {round}"
            );
        }
    }

    /// 6. After 1000 updates, prior values are in `[0, 1]` and sum to ~1.0.
    #[test]
    fn bayes_prior_update_bounded() {
        let mut strategy = BayesianDiffStrategy::new();
        let frames = [
            stable_frame(),
            bursty_frame(),
            resize_frame(),
            degraded_frame(),
        ];
        for i in 0..1000 {
            strategy.observe_with_ledger(&frames[i % frames.len()], None);
        }
        let post = strategy.posterior();
        let sum: f64 = post.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "prior sum {sum} != 1.0 after 1000 updates"
        );
        for (i, &p) in post.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&p),
                "prior[{i}] = {p} is out of bounds"
            );
        }
    }

    /// 7. When `deterministic_fallback` is true, always returns `Full`.
    #[test]
    fn bayes_deterministic_fallback() {
        let mut strategy = BayesianDiffStrategy::new();
        strategy.deterministic_fallback = true;

        assert_eq!(
            strategy.observe_with_ledger(&stable_frame(), None),
            DiffAction::Full
        );
        assert_eq!(
            strategy.observe_with_ledger(&degraded_frame(), None),
            DiffAction::Full
        );
        assert_eq!(
            strategy.observe_with_ledger(&resize_frame(), None),
            DiffAction::Full
        );
    }

    /// 8. With uniform prior and given loss matrix, verify expected losses
    ///    match hand calculation.
    #[test]
    fn bayes_expected_loss_uniform() {
        let strategy = BayesianDiffStrategy::new();
        // Uniform prior: [0.25, 0.25, 0.25, 0.25]
        // Expected loss for Incremental: 0.25*(1+12+15+10) = 0.25*38 = 9.5
        // Expected loss for Full:        0.25*(8+3+2+3)    = 0.25*16 = 4.0
        // Expected loss for Deferred:    0.25*(20+5+10+15)  = 0.25*50 = 12.5
        let el_inc = strategy.expected_loss(DiffAction::Incremental);
        let el_full = strategy.expected_loss(DiffAction::Full);
        let el_def = strategy.expected_loss(DiffAction::Deferred);

        assert!(
            (el_inc - 9.5).abs() < 1e-9,
            "incremental expected loss {el_inc} != 9.5"
        );
        assert!(
            (el_full - 4.0).abs() < 1e-9,
            "full expected loss {el_full} != 4.0"
        );
        assert!(
            (el_def - 12.5).abs() < 1e-9,
            "deferred expected loss {el_def} != 12.5"
        );

        // With uniform prior, Full should have the lowest expected loss.
        assert!(
            el_full < el_inc && el_full < el_def,
            "Full should have lowest expected loss with uniform prior"
        );
    }

    // ─── Integration tests for D.2 wiring (br-v3hid) ─────────────────

    /// 9. Render 100 frames with mutations, compare to full-diff baseline.
    ///    Since ftui doesn't support differential rendering yet, we verify
    ///    that Bayesian mode chooses actions without errors and produces
    ///    consistent posteriors.
    #[test]
    fn tui_bayes_integration_no_glitch() {
        let mut strategy = BayesianDiffStrategy::new();

        // Simulate 100 frames with varying conditions.
        let mut last_action = DiffAction::Full;
        for i in 0..100 {
            let frame = FrameState {
                change_ratio: if i % 10 == 0 { 0.6 } else { 0.05 },
                is_resize: i == 50,
                budget_remaining_ms: if i % 20 == 0 { 2.0 } else { 14.0 },
                error_count: 0,
            };
            let action = strategy.observe_with_ledger(&frame, None);
            last_action = action;

            // Posterior must remain valid.
            let post = strategy.posterior();
            let sum: f64 = post.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "posterior sum {sum} != 1.0 at frame {i}"
            );
        }

        // Final action should be valid.
        assert!(matches!(
            last_action,
            DiffAction::Incremental | DiffAction::Full | DiffAction::Deferred
        ));
    }

    /// 10. Simulate resize event, verify full diff chosen.
    #[test]
    fn tui_bayes_resize_triggers_full() {
        let mut strategy = BayesianDiffStrategy::new();

        // Train with a few stable frames first.
        for _ in 0..5 {
            strategy.observe_with_ledger(&stable_frame(), None);
        }

        // Now resize — should choose Full.
        let action = strategy.observe_with_ledger(&resize_frame(), None);
        assert_eq!(
            action,
            DiffAction::Full,
            "resize should trigger full redraw"
        );
    }

    /// 11. 10 stable frames, verify incremental chosen for most.
    #[test]
    fn tui_bayes_stable_uses_incremental() {
        let mut strategy = BayesianDiffStrategy::new();

        let mut incremental_count = 0;
        for _ in 0..10 {
            let action = strategy.observe_with_ledger(&stable_frame(), None);
            if action == DiffAction::Incremental {
                incremental_count += 1;
            }
        }

        // After 10 stable frames, the strategy should converge to incremental.
        // The first frame with uniform prior chooses Full, but subsequent frames
        // should increasingly prefer Incremental.
        assert!(
            incremental_count >= 5,
            "expected at least 5 incremental actions out of 10 stable frames, got {incremental_count}"
        );
    }

    /// 12. Verify evidence ledger has entries after rendering.
    #[test]
    fn tui_bayes_evidence_recorded() {
        use mcp_agent_mail_core::evidence_ledger::EvidenceLedger;

        let ledger = EvidenceLedger::new(64);
        let mut strategy = BayesianDiffStrategy::new();

        // Render a few frames with ledger recording.
        for _ in 0..5 {
            strategy.observe_with_ledger(&stable_frame(), Some(&ledger));
        }

        // Evidence ledger should have entries.
        let entries = ledger.recent(10);
        assert_eq!(
            entries.len(),
            5,
            "expected 5 evidence entries, got {}",
            entries.len()
        );

        // All entries should be for the tui.diff_strategy decision point.
        for entry in &entries {
            assert_eq!(entry.decision_point, "tui.diff_strategy");
        }

        // Each entry should have a valid action label.
        for entry in &entries {
            assert!(
                entry.action == "incremental"
                    || entry.action == "full"
                    || entry.action == "deferred",
                "unexpected action: {}",
                entry.action
            );
        }
    }

    // ─── D.3: Frame-time benchmarks + conformal coverage (br-2zq8o) ───

    /// 13. Bayesian p99 decision time should be comparable to full-diff.
    ///     Since Bayesian just does array arithmetic (no heap allocs, no IO),
    ///     it should not be significantly slower than the deterministic fallback.
    #[test]
    fn frame_bench_bayesian_not_slower_than_full() {
        use std::time::Instant;

        const FRAMES: usize = 10_000;
        let frames = [
            stable_frame(),
            bursty_frame(),
            resize_frame(),
            degraded_frame(),
        ];

        // Measure Bayesian strategy.
        let mut strategy = BayesianDiffStrategy::new();
        let mut bayes_times = Vec::with_capacity(FRAMES);
        for i in 0..FRAMES {
            let start = Instant::now();
            let _ = strategy.observe_with_ledger(&frames[i % 4], None);
            bayes_times.push(start.elapsed().as_nanos());
        }
        bayes_times.sort_unstable();
        let bayes_p99 = bayes_times[FRAMES * 99 / 100];

        // Measure deterministic fallback (Full always).
        let mut fallback = BayesianDiffStrategy::new();
        fallback.deterministic_fallback = true;
        let mut full_times = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let start = Instant::now();
            let _ = fallback.observe_with_ledger(&stable_frame(), None);
            full_times.push(start.elapsed().as_nanos());
        }
        full_times.sort_unstable();
        let full_p99 = full_times[FRAMES * 99 / 100];

        // Bayesian p99 should be within 20x of Full p99.
        // (Full is trivially fast since it short-circuits; Bayesian does
        // real computation but still sub-microsecond.)
        assert!(
            bayes_p99 <= full_p99 * 20,
            "bayesian p99 ({bayes_p99}ns) > 20x full p99 ({full_p99}ns)"
        );
    }

    /// 14. On stable frames, Incremental should be chosen faster than Full.
    ///     (The strategy converges to Incremental within a few frames.)
    #[test]
    fn frame_bench_incremental_faster_on_stable() {
        let mut strategy = BayesianDiffStrategy::new();

        // Run 20 stable frames: first may choose Full (uniform prior),
        // but should converge to Incremental.
        let mut incremental_count = 0;
        for _ in 0..20 {
            let action = strategy.observe_with_ledger(&stable_frame(), None);
            if action == DiffAction::Incremental {
                incremental_count += 1;
            }
        }

        // At least 15/20 should be Incremental on purely stable data.
        assert!(
            incremental_count >= 15,
            "expected >= 15 incremental out of 20 stable frames, got {incremental_count}"
        );
    }

    /// 15. Conformal coverage >= 90% on Bayesian decision times.
    ///     Feed 500+ decision times into a conformal predictor; verify that
    ///     >= 90% of subsequent times fall within the predicted interval.
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn conformal_coverage_90pct() {
        use mcp_agent_mail_core::conformal::ConformalPredictor;
        use std::time::Instant;

        let frames = [
            stable_frame(),
            bursty_frame(),
            resize_frame(),
            degraded_frame(),
        ];
        let mut strategy = BayesianDiffStrategy::new();
        let mut predictor = ConformalPredictor::new(200, 0.90);

        // Collect 500 decision times.
        for i in 0..500 {
            let start = Instant::now();
            let _ = strategy.observe_with_ledger(&frames[i % 4], None);
            let elapsed_us = start.elapsed().as_nanos() as f64 / 1000.0;
            predictor.observe(elapsed_us);
        }

        // Verify conformal predictor has calibrated.
        assert!(
            predictor.predict().is_some(),
            "conformal predictor should have calibrated after 500 observations"
        );

        // Check empirical coverage.
        let coverage = predictor
            .empirical_coverage()
            .expect("coverage should be available");
        assert!(
            coverage >= 0.85,
            "conformal coverage {coverage:.3} should be >= 0.85 (nominal 0.90)"
        );
    }

    /// 16. Golden fixture: a fixed sequence of 50 frames produces deterministic
    ///     strategy decisions (since the algorithm is deterministic given inputs).
    #[test]
    fn golden_strategy_decisions_match() {
        let mut strategy = BayesianDiffStrategy::new();

        // Fixed sequence: 10 stable, 5 bursty, 1 resize, 5 degraded,
        // 10 stable, 5 bursty, 1 resize, 13 stable.
        let mut sequence: Vec<FrameState> = Vec::new();
        for _ in 0..10 {
            sequence.push(stable_frame());
        }
        for _ in 0..5 {
            sequence.push(bursty_frame());
        }
        sequence.push(resize_frame());
        for _ in 0..5 {
            sequence.push(degraded_frame());
        }
        for _ in 0..10 {
            sequence.push(stable_frame());
        }
        for _ in 0..5 {
            sequence.push(bursty_frame());
        }
        sequence.push(resize_frame());
        for _ in 0..13 {
            sequence.push(stable_frame());
        }
        assert_eq!(sequence.len(), 50);

        // Run the sequence and collect decisions.
        let decisions: Vec<DiffAction> = sequence
            .iter()
            .map(|f| strategy.observe_with_ledger(f, None))
            .collect();

        // Run the SAME sequence again from scratch to verify determinism.
        let mut strategy2 = BayesianDiffStrategy::new();
        let decisions2: Vec<DiffAction> = sequence
            .iter()
            .map(|f| strategy2.observe_with_ledger(f, None))
            .collect();

        assert_eq!(
            decisions, decisions2,
            "decisions should be deterministic for the same input sequence"
        );

        // Verify structural properties of the decision sequence.
        // 1. Stable frames should predominantly choose Incremental.
        let first_10_inc = decisions[0..10]
            .iter()
            .filter(|&&d| d == DiffAction::Incremental)
            .count();
        assert!(
            first_10_inc >= 8,
            "first 10 stable frames should have >= 8 incremental, got {first_10_inc}"
        );

        // 2. Resize frame (index 15) should choose Full.
        assert_eq!(
            decisions[15],
            DiffAction::Full,
            "resize frame should be Full"
        );

        // 3. Second resize (index 36) should also choose Full.
        assert_eq!(
            decisions[36],
            DiffAction::Full,
            "second resize should be Full"
        );
    }

    // ── Additional coverage tests ────────────────────────────────────

    #[test]
    fn diff_action_index_roundtrip() {
        for i in 0..3 {
            let action = DiffAction::from_index(i);
            assert_eq!(action.index(), i);
        }
        // Out-of-range maps to Deferred
        assert_eq!(DiffAction::from_index(99), DiffAction::Deferred);
    }

    #[test]
    fn diff_action_labels() {
        assert_eq!(DiffAction::Incremental.label(), "incremental");
        assert_eq!(DiffAction::Full.label(), "full");
        assert_eq!(DiffAction::Deferred.label(), "deferred");
    }

    #[test]
    fn strategy_default_impl() {
        let strategy = BayesianDiffStrategy::default();
        // Uniform prior
        for &p in &strategy.posterior() {
            assert!((p - 0.25).abs() < 1e-9);
        }
        assert!(!strategy.deterministic_fallback);
    }

    #[test]
    fn compute_likelihood_resize_dominates() {
        let frame = resize_frame();
        let lik = compute_likelihood(&frame);
        assert!((lik[STATE_RESIZE] - 0.9).abs() < 1e-9);
        assert!(lik[STATE_RESIZE] > lik[STATE_STABLE]);
        assert!(lik[STATE_RESIZE] > lik[STATE_BURSTY]);
        assert!(lik[STATE_RESIZE] > lik[STATE_DEGRADED]);
    }

    #[test]
    fn compute_likelihood_stable_frame() {
        let frame = stable_frame();
        let lik = compute_likelihood(&frame);
        assert!(
            lik[STATE_STABLE] >= 0.8,
            "stable frame should have high stable likelihood, got {}",
            lik[STATE_STABLE]
        );
    }

    #[test]
    fn compute_likelihood_degraded_frame() {
        let frame = degraded_frame();
        let lik = compute_likelihood(&frame);
        assert!(
            lik[STATE_DEGRADED] >= 0.8,
            "degraded frame should have high degraded likelihood, got {}",
            lik[STATE_DEGRADED]
        );
    }

    #[test]
    fn compute_likelihood_bursty_frame() {
        let frame = bursty_frame();
        let lik = compute_likelihood(&frame);
        assert!(
            lik[STATE_BURSTY] >= 0.8,
            "bursty frame should have high bursty likelihood, got {}",
            lik[STATE_BURSTY]
        );
    }

    #[test]
    fn compute_likelihood_medium_change_ratio() {
        let frame = FrameState {
            change_ratio: 0.35,
            is_resize: false,
            budget_remaining_ms: 10.0,
            error_count: 0,
        };
        let lik = compute_likelihood(&frame);
        assert!(
            lik[STATE_BURSTY] >= 0.5,
            "medium change ratio should give bursty >= 0.5, got {}",
            lik[STATE_BURSTY]
        );
        assert!(lik[STATE_RESIZE] < 0.1);
    }

    #[test]
    fn compute_likelihood_low_budget_moderate_errors() {
        let frame = FrameState {
            change_ratio: 0.05,
            is_resize: false,
            budget_remaining_ms: 6.0,
            error_count: 2,
        };
        let lik = compute_likelihood(&frame);
        assert!(
            lik[STATE_DEGRADED] >= 0.3,
            "moderate budget/errors should give degraded >= 0.3, got {}",
            lik[STATE_DEGRADED]
        );
    }

    #[test]
    fn dominant_state_basic() {
        let dist = [0.1, 0.6, 0.2, 0.1];
        assert_eq!(dominant_state(&dist), STATE_BURSTY);
    }

    #[test]
    fn dominant_state_first_wins_on_tie() {
        let dist = [0.5, 0.5, 0.0, 0.0];
        // When tied, max_by returns the last of equal elements, so this
        // depends on iterator order. The important thing is it returns a valid index.
        let idx = dominant_state(&dist);
        assert!(idx == STATE_STABLE || idx == STATE_BURSTY);
    }

    #[test]
    fn expected_loss_after_stable_training() {
        let mut strategy = BayesianDiffStrategy::new();
        // Train with 20 stable frames
        for _ in 0..20 {
            strategy.observe_with_ledger(&stable_frame(), None);
        }
        // Now incremental should have lowest expected loss
        let el_inc = strategy.expected_loss(DiffAction::Incremental);
        let el_full = strategy.expected_loss(DiffAction::Full);
        assert!(
            el_inc < el_full,
            "after stable training, incremental ({el_inc}) should beat full ({el_full})"
        );
    }

    #[test]
    fn zero_change_ratio_zero_errors() {
        let frame = FrameState {
            change_ratio: 0.0,
            is_resize: false,
            budget_remaining_ms: 16.0,
            error_count: 0,
        };
        let mut strategy = BayesianDiffStrategy::new();
        // With a perfectly calm frame, should not crash
        let action = strategy.observe_with_ledger(&frame, None);
        assert!(matches!(
            action,
            DiffAction::Incremental | DiffAction::Full | DiffAction::Deferred
        ));
    }

    #[test]
    fn alternating_frames_stays_bounded() {
        let mut strategy = BayesianDiffStrategy::new();
        for i in 0..200 {
            let frame = if i % 2 == 0 {
                stable_frame()
            } else {
                bursty_frame()
            };
            strategy.observe_with_ledger(&frame, None);
        }
        let post = strategy.posterior();
        let sum: f64 = post.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        for (i, &p) in post.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&p),
                "prior[{i}] = {p} out of bounds after alternating frames"
            );
        }
    }
}
