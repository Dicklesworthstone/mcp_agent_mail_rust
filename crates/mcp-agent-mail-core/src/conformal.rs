//! Conformal Prediction for distribution-free prediction intervals.
//!
//! Given a stream of observations, produces prediction intervals with
//! guaranteed coverage: the next observation falls within the interval
//! with probability >= `coverage`, regardless of the underlying distribution.
//!
//! Uses split conformal prediction with a sliding calibration window.
//!
//! # References
//!
//! Vovk, V., Gammerman, A., & Shafer, G. (2005). *Algorithmic Learning
//! in a Random World*. Springer.

use std::collections::VecDeque;

/// Minimum calibration window size before predictions are emitted.
const MIN_CALIBRATION: usize = 30;

/// A prediction interval with coverage guarantee.
#[derive(Debug, Clone)]
pub struct PredictionInterval {
    /// Lower bound of the interval.
    pub lower: f64,
    /// Upper bound of the interval.
    pub upper: f64,
    /// Nominal coverage level (1 - alpha).
    pub coverage: f64,
    /// Number of calibration observations used.
    pub calibration_size: usize,
}

/// Distribution-free conformal predictor using nonconformity scores.
///
/// Maintains a sliding window of recent observations and produces
/// prediction intervals with guaranteed finite-sample coverage.
///
/// # Example
///
/// ```
/// use mcp_agent_mail_core::conformal::ConformalPredictor;
///
/// let mut predictor = ConformalPredictor::new(100, 0.90);
///
/// // Calibrate with 50 observations
/// for i in 0..50 {
///     predictor.observe(i as f64 * 0.1);
/// }
///
/// // Get prediction interval
/// if let Some(interval) = predictor.predict() {
///     assert!(interval.coverage >= 0.90);
/// }
/// ```
pub struct ConformalPredictor {
    /// Recent observations (sliding window).
    observations: VecDeque<f64>,
    /// Maximum calibration window size.
    window: usize,
    /// Coverage level (1 - alpha), e.g., 0.90 for 90% coverage.
    coverage: f64,
    /// Total observations seen (including those that fell out of window).
    total_count: usize,
    /// Count of observations that fell within the predicted interval.
    hits: usize,
    /// Count of observations for which a prediction was available.
    predictions_made: usize,
}

impl ConformalPredictor {
    /// Create a new conformal predictor.
    ///
    /// - `window`: maximum number of recent observations to keep for
    ///   calibration. Larger windows give tighter intervals but adapt
    ///   more slowly to distribution shifts.
    /// - `coverage`: nominal coverage level (e.g., 0.90 for 90%).
    ///   Must be in (0, 1).
    #[must_use]
    pub fn new(window: usize, coverage: f64) -> Self {
        Self {
            observations: VecDeque::with_capacity(window.min(1024)),
            window,
            coverage,
            total_count: 0,
            hits: 0,
            predictions_made: 0,
        }
    }

    /// Observe a new data point and add it to the calibration window.
    ///
    /// Also tracks coverage: if a prediction interval was available
    /// before this observation, checks whether the observation fell
    /// within it.
    pub fn observe(&mut self, x: f64) {
        // Track coverage before adding the new observation.
        if let Some(interval) = self.predict() {
            self.predictions_made += 1;
            if x >= interval.lower && x <= interval.upper {
                self.hits += 1;
            }
        }

        // Add to window.
        if self.observations.len() >= self.window {
            self.observations.pop_front();
        }
        self.observations.push_back(x);
        self.total_count += 1;
    }

    /// Compute a prediction interval for the next observation.
    ///
    /// Returns `None` if the calibration window has fewer than
    /// `MIN_CALIBRATION` (30) observations.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn predict(&self) -> Option<PredictionInterval> {
        let n = self.observations.len();
        if n < MIN_CALIBRATION {
            return None;
        }

        // Compute the median of the calibration window.
        let median = self.median();

        // Compute nonconformity scores: |obs - median|.
        let mut scores: Vec<f64> = self
            .observations
            .iter()
            .map(|&x| (x - median).abs())
            .collect();

        // The conformal quantile: ceil((n+1) * coverage) / n.
        // This ensures finite-sample coverage >= nominal coverage.
        let quantile_idx = ((n as f64 + 1.0) * self.coverage).ceil() as usize;
        let quantile_idx = quantile_idx.min(n) - 1; // 0-indexed, capped at n-1

        let (_, q, _) = scores.select_nth_unstable_by(quantile_idx, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let q = *q;

        Some(PredictionInterval {
            lower: median - q,
            upper: median + q,
            coverage: self.coverage,
            calibration_size: n,
        })
    }

    /// Compute the median of the calibration window.
    fn median(&self) -> f64 {
        let mut values: Vec<f64> = self.observations.iter().copied().collect();
        let n = values.len();
        if n == 0 {
            return 0.0;
        }

        let mid_idx = n / 2;
        let (_, median, _) = values.select_nth_unstable_by(mid_idx, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let median_val = *median;

        if n.is_multiple_of(2) {
            // Even number of elements: median is avg of sorted[mid-1] and sorted[mid].
            // select_nth_unstable puts element at mid_idx in place, and everything
            // smaller to the left. The max of the left partition is sorted[mid-1].
            let left_max = values[..mid_idx]
                .iter()
                .copied()
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(median_val);

            f64::midpoint(left_max, median_val)
        } else {
            median_val
        }
    }

    /// Number of observations in the calibration window.
    #[must_use]
    pub fn calibration_size(&self) -> usize {
        self.observations.len()
    }

    /// Total observations seen (including those evicted from window).
    #[must_use]
    pub const fn total_observations(&self) -> usize {
        self.total_count
    }

    /// Empirical coverage: fraction of predictions that contained the
    /// actual observation. Returns `None` if no predictions have been made.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn empirical_coverage(&self) -> Option<f64> {
        if self.predictions_made == 0 {
            return None;
        }
        Some(self.hits as f64 / self.predictions_made as f64)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Coverage guarantee: >= 90% of N(0,1) observations fall within
    /// the predicted interval.
    #[test]
    fn conformal_coverage_guarantee() {
        let mut predictor = ConformalPredictor::new(200, 0.90);

        // Simple deterministic pseudo-normal: alternate between a set of
        // values that approximate N(0,1) quantiles.
        let pseudo_normal: Vec<f64> = (0..10000)
            .map(|i| {
                // Deterministic spread around 0 with varying magnitudes.
                let phase = f64::from(i) * 0.1;
                phase.sin() * 2.0
            })
            .collect();

        for &x in &pseudo_normal {
            predictor.observe(x);
        }

        let coverage = predictor.empirical_coverage().unwrap();
        assert!(
            coverage >= 0.88,
            "empirical coverage {coverage:.3} should be >= 0.88 (nominal 0.90)"
        );
    }

    /// Window size is respected: `calibration_size` never exceeds window.
    #[test]
    fn conformal_window_size_respected() {
        let mut predictor = ConformalPredictor::new(100, 0.90);

        for i in 0..200 {
            predictor.observe(f64::from(i));
        }

        assert_eq!(
            predictor.calibration_size(),
            100,
            "calibration size should be capped at window=100"
        );
        assert_eq!(predictor.total_observations(), 200);
    }

    /// Returns None when calibration is insufficient (< 30 observations).
    #[test]
    fn conformal_none_when_insufficient() {
        let mut predictor = ConformalPredictor::new(100, 0.90);

        // Less than MIN_CALIBRATION observations.
        for i in 0..29 {
            predictor.observe(f64::from(i));
        }
        assert!(
            predictor.predict().is_none(),
            "should return None with only 29 observations"
        );

        // At exactly MIN_CALIBRATION, prediction should be available.
        predictor.observe(29.0);
        assert!(
            predictor.predict().is_some(),
            "should return Some with 30 observations"
        );
    }

    /// Intervals adapt after a distribution shift.
    #[test]
    fn conformal_adapts_to_distribution_shift() {
        let mut predictor = ConformalPredictor::new(100, 0.90);

        // 100 observations from a distribution centered at 0.
        for i in 0..100 {
            let x = (f64::from(i) * 0.5).sin();
            predictor.observe(x);
        }

        let interval_before = predictor.predict().unwrap();

        // 100 observations from a distribution centered at 10.
        for i in 0..100 {
            let x = 10.0 + (f64::from(i) * 0.5).sin();
            predictor.observe(x);
        }

        let interval_after = predictor.predict().unwrap();

        // After the shift, the interval center should have moved toward 10.
        let center_before = f64::midpoint(interval_before.lower, interval_before.upper);
        let center_after = f64::midpoint(interval_after.lower, interval_after.upper);

        assert!(
            center_after > center_before + 5.0,
            "interval center should shift: before={center_before:.2}, after={center_after:.2}"
        );
    }

    /// Constant data should produce a zero-width (or near-zero) interval.
    #[test]
    fn conformal_constant_data_narrow_interval() {
        let mut predictor = ConformalPredictor::new(100, 0.90);
        for _ in 0..50 {
            predictor.observe(42.0);
        }
        let interval = predictor.predict().unwrap();
        let width = interval.upper - interval.lower;
        assert!(
            width < 1e-10,
            "constant data should produce near-zero-width interval, got width={width}"
        );
    }

    /// `empirical_coverage()` returns None before any predictions are made.
    #[test]
    fn conformal_empirical_coverage_none_before_predictions() {
        let predictor = ConformalPredictor::new(100, 0.90);
        assert!(predictor.empirical_coverage().is_none());
    }

    /// `calibration_size()` and `total_observations()` getters.
    #[test]
    fn conformal_getters() {
        let mut predictor = ConformalPredictor::new(50, 0.90);
        assert_eq!(predictor.calibration_size(), 0);
        assert_eq!(predictor.total_observations(), 0);

        for i in 0..75 {
            predictor.observe(f64::from(i));
        }
        assert_eq!(predictor.calibration_size(), 50); // capped at window
        assert_eq!(predictor.total_observations(), 75);
    }

    /// `PredictionInterval` Debug and Clone derives.
    #[test]
    fn prediction_interval_debug_clone() {
        let mut predictor = ConformalPredictor::new(100, 0.95);
        for i in 0..40 {
            predictor.observe(f64::from(i));
        }
        let interval = predictor.predict().unwrap();
        let cloned = interval.clone();
        assert!((cloned.coverage - interval.coverage).abs() < f64::EPSILON);
        assert_eq!(cloned.calibration_size, interval.calibration_size);
        let debug = format!("{interval:?}");
        assert!(debug.contains("coverage"));
    }

    /// Median with even-count window.
    #[test]
    fn conformal_even_calibration_count() {
        let mut predictor = ConformalPredictor::new(100, 0.90);
        // Feed exactly 30 values (even count)
        for i in 0..30 {
            predictor.observe(f64::from(i));
        }
        let interval = predictor.predict().unwrap();
        assert_eq!(interval.calibration_size, 30);
        assert!(interval.lower < interval.upper);
    }

    /// Coverage holds for heavy-tailed data when presented in shuffled
    /// (approximately exchangeable) order.
    /// Conformal prediction is distribution-free for exchangeable data.
    #[test]
    #[allow(clippy::cast_sign_loss, clippy::cast_precision_loss)]
    fn conformal_coverage_for_heavy_tailed() {
        let mut predictor = ConformalPredictor::new(200, 0.90);

        // Generate pseudo-random heavy-tailed values using a hash-based
        // scramble to ensure approximate exchangeability.
        let n = 5000;
        let data: Vec<f64> = (0..n)
            .map(|i| {
                // Simple hash-based pseudo-random: maps i to a deterministic
                // but well-distributed value.
                let h = (i as u64).wrapping_mul(2_654_435_761).wrapping_add(13) % 10000;
                let u = (h as f64 + 0.5) / 10001.0; // (0, 1)
                                                    // Map through tan for heavy tails, but clamp to avoid infinity.
                let angle = (u - 0.5) * std::f64::consts::PI * 0.95;
                angle.tan()
            })
            .collect();

        for &x in &data {
            predictor.observe(x);
        }

        let coverage = predictor.empirical_coverage().unwrap();
        assert!(
            coverage >= 0.85,
            "empirical coverage {coverage:.3} on heavy-tailed data should be >= 0.85 (nominal 0.90)"
        );
    }

    /// Prediction interval contains the correct coverage metadata.
    #[test]
    #[allow(clippy::cast_sign_loss, clippy::cast_precision_loss)]
    fn conformal_interval_metadata() {
        let mut predictor = ConformalPredictor::new(500, 0.90);

        // Feed stable data.
        for i in 0..100 {
            // Use hash-based scramble for stable pseudo-random values.
            let h = (i as u64).wrapping_mul(2_654_435_761) % 1000;
            predictor.observe((h as f64 / 1000.0).mul_add(0.2, 5.0) - 0.1);
        }

        let interval = predictor.predict().unwrap();
        assert!(
            (interval.coverage - 0.90).abs() < 1e-10,
            "coverage should be 0.90"
        );
        assert_eq!(interval.calibration_size, 100);
        assert!(
            interval.lower < interval.upper,
            "lower ({}) should be < upper ({})",
            interval.lower,
            interval.upper
        );
        // Interval should be centered near 5.0.
        let center = f64::midpoint(interval.lower, interval.upper);
        assert!(
            (center - 5.0).abs() < 0.5,
            "center should be near 5.0, got {center}"
        );
    }
}
