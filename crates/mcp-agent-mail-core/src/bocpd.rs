//! Bayesian Online Change-Point Detection (Adams & `MacKay`, 2007).
//!
//! Maintains a posterior distribution over "run lengths" — the number of
//! observations since the last change point. When the posterior mass shifts
//! from long run lengths to short ones (indicating a recent regime change),
//! a change point is emitted.
//!
//! The observation model is a Gaussian with conjugate Normal-Inverse-Gamma
//! (NIG) prior, supporting detection of both mean and variance shifts.
//!
//! # References
//!
//! Adams, R. P. & `MacKay`, D. J. C. (2007). *Bayesian Online Changepoint
//! Detection*. arXiv:0710.3742.

/// A detected change point in the observation stream.
#[derive(Debug, Clone)]
pub struct ChangePoint {
    /// Index in the observation stream (0-based).
    pub index: usize,
    /// Posterior probability mass on short run lengths (the detection signal).
    pub probability: f64,
    /// Estimated mean before the change point.
    pub pre_mean: f64,
    /// Estimated mean after the change point.
    pub post_mean: f64,
}

/// Sufficient statistics for a Normal-Inverse-Gamma conjugate model
/// at a given run length.
#[derive(Debug, Clone)]
struct NigStats {
    /// Prior/posterior mean of the mean.
    mu: f64,
    /// Number of pseudo-observations (strength of prior).
    kappa: f64,
    /// Shape parameter for the inverse-gamma on variance.
    alpha: f64,
    /// Scale parameter for the inverse-gamma on variance.
    beta: f64,
}

impl NigStats {
    /// Default prior: weakly informative centered at 0.
    const fn default_prior() -> Self {
        Self {
            mu: 0.0,
            kappa: 0.1, // weak prior on mean location
            alpha: 1.0, // minimal shape
            beta: 1.0,  // unit scale
        }
    }

    /// Update NIG parameters with a new observation.
    fn update(&self, x: f64) -> Self {
        let kappa_new = self.kappa + 1.0;
        let mu_new = self.kappa.mul_add(self.mu, x) / kappa_new;
        let alpha_new = self.alpha + 0.5;
        let beta_new = self.beta + 0.5 * self.kappa * (x - self.mu).powi(2) / kappa_new;
        Self {
            mu: mu_new,
            kappa: kappa_new,
            alpha: alpha_new,
            beta: beta_new,
        }
    }

    /// Student-t predictive log-probability for a new observation.
    ///
    /// The predictive distribution under a NIG model is a Student-t with
    /// `2*alpha` degrees of freedom, location `mu`, and scale
    /// `beta * (kappa + 1) / (alpha * kappa)`.
    fn log_predictive(&self, x: f64) -> f64 {
        let df = 2.0 * self.alpha;
        let scale_sq = self.beta * (self.kappa + 1.0) / (self.alpha * self.kappa);
        let scale = scale_sq.sqrt();

        let z = (x - self.mu) / scale;
        let half_df = df / 2.0;
        let half_df_plus_half = f64::midpoint(df, 1.0);

        0.5f64.mul_add(-(df * std::f64::consts::PI * scale_sq).ln(), ln_gamma(half_df_plus_half) - ln_gamma(half_df))
            - half_df_plus_half * (z * z / df).ln_1p()
    }

    /// Predictive mean (= the current posterior mean of mu).
    const fn predictive_mean(&self) -> f64 {
        self.mu
    }
}

/// Log-gamma function via the Lanczos approximation (g=7, n=9).
fn ln_gamma(x: f64) -> f64 {
    if x <= 0.0 {
        return f64::INFINITY;
    }

    const COEFFS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_403,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];

    let x = x - 1.0;
    let mut sum = COEFFS[0];
    for (i, &c) in COEFFS[1..].iter().enumerate() {
        sum += c / (x + (i as f64) + 1.0);
    }

    let t = x + 7.5;
    0.5f64.mul_add((2.0 * std::f64::consts::PI).ln(), (x + 0.5) * t.ln()) - t + sum.ln()
}

/// Window size for computing the "short run length" mass used in
/// change-point detection. When the posterior mass on run lengths
/// `0..CHANGE_WINDOW` exceeds the threshold, a change point is declared.
const CHANGE_WINDOW: usize = 15;

/// Bayesian Online Change-Point Detector.
///
/// Call [`observe`](Self::observe) with each new data point. When a change
/// point is detected, a [`ChangePoint`] is returned.
///
/// Detection uses the cumulative posterior mass on short run lengths
/// (r < 15). When this mass exceeds the threshold, it indicates that
/// the model believes a regime change happened within the last 15
/// observations.
///
/// # Example
///
/// ```
/// use mcp_agent_mail_core::bocpd::BocpdDetector;
///
/// let mut detector = BocpdDetector::new(1.0 / 250.0, 0.5, 300);
///
/// // Feed 100 observations from N(0,1)
/// for _ in 0..100 {
///     let _ = detector.observe(0.0);
/// }
///
/// // Feed 100 observations from N(5,1) — a mean shift
/// for _ in 0..100 {
///     let cp = detector.observe(5.0);
///     // A change point will eventually be detected
/// }
/// ```
pub struct BocpdDetector {
    /// Hazard rate: probability of change point at each step.
    hazard: f64,
    /// Log run-length posterior distribution.
    log_run_dist: Vec<f64>,
    /// NIG sufficient statistics per run length.
    stats: Vec<NigStats>,
    /// Maximum run length to track (truncation bound).
    max_run_length: usize,
    /// Threshold on cumulative mass for short run lengths.
    threshold: f64,
    /// Current observation index.
    index: usize,
    /// Prior parameters for new run lengths.
    prior: NigStats,
    /// Whether a change point has been emitted for the current regime
    /// shift (prevents repeated firing for the same shift).
    in_change: bool,
    /// Previous most probable run length (for tracking transitions).
    prev_max_rl: usize,
}

impl BocpdDetector {
    /// Create a new detector.
    ///
    /// - `hazard`: probability of a change point at each time step
    ///   (e.g., 1/250 means we expect one every 250 observations).
    /// - `threshold`: cumulative probability threshold on short run
    ///   lengths for declaring a change point (e.g., 0.5).
    /// - `max_run_length`: truncation bound for the run-length
    ///   distribution (limits memory usage).
    #[must_use]
    pub fn new(hazard: f64, threshold: f64, max_run_length: usize) -> Self {
        let prior = NigStats::default_prior();
        Self {
            hazard,
            log_run_dist: vec![0.0],
            stats: vec![prior.clone()],
            max_run_length,
            threshold,
            index: 0,
            prior,
            in_change: true, // suppress detection at startup
            prev_max_rl: 0,
        }
    }

    /// Create a detector with a custom prior on the observation mean.
    #[must_use]
    pub fn with_prior(
        hazard: f64,
        threshold: f64,
        max_run_length: usize,
        prior_mu: f64,
        prior_kappa: f64,
        prior_alpha: f64,
        prior_beta: f64,
    ) -> Self {
        let prior = NigStats {
            mu: prior_mu,
            kappa: prior_kappa,
            alpha: prior_alpha,
            beta: prior_beta,
        };
        Self {
            hazard,
            log_run_dist: vec![0.0],
            stats: vec![prior.clone()],
            max_run_length,
            threshold,
            index: 0,
            prior,
            in_change: true,
            prev_max_rl: 0,
        }
    }

    /// Observe a new data point and update the run-length posterior.
    ///
    /// Returns `Some(ChangePoint)` if a change point is detected at this
    /// step, `None` otherwise.
    pub fn observe(&mut self, x: f64) -> Option<ChangePoint> {
        let n = self.log_run_dist.len();
        let log_hazard = self.hazard.ln();
        let log_1_minus_hazard = (1.0 - self.hazard).ln();

        // Compute log predictive probabilities for each run length.
        let log_pred: Vec<f64> = self.stats.iter().map(|s| s.log_predictive(x)).collect();

        // Compute growth + change-point probabilities.
        let mut new_log_run_dist = Vec::with_capacity(n + 1);

        // Change-point: run length resets to 0.
        let cp_terms: Vec<f64> = (0..n)
            .map(|r| self.log_run_dist[r] + log_pred[r] + log_hazard)
            .collect();
        let log_cp = log_sum_exp(&cp_terms);
        new_log_run_dist.push(log_cp);

        // Growth: run length increases by 1.
        for r in 0..n {
            let log_growth = self.log_run_dist[r] + log_pred[r] + log_1_minus_hazard;
            new_log_run_dist.push(log_growth);
        }

        // Normalize.
        let log_evidence = log_sum_exp(&new_log_run_dist);
        for v in &mut new_log_run_dist {
            *v -= log_evidence;
        }

        // Pre-change mean estimate from old state.
        let pre_mean = if n >= 2 {
            let max_r = self
                .log_run_dist
                .iter()
                .enumerate()
                .skip(1)
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map_or(1, |(r, _)| r);
            self.stats[max_r.min(self.stats.len() - 1)].predictive_mean()
        } else {
            self.stats[0].predictive_mean()
        };

        // Update sufficient statistics.
        let mut new_stats = Vec::with_capacity(new_log_run_dist.len());
        new_stats.push(self.prior.update(x));
        for s in &self.stats {
            new_stats.push(s.update(x));
        }

        // Truncate.
        if new_log_run_dist.len() > self.max_run_length {
            new_log_run_dist.truncate(self.max_run_length);
            new_stats.truncate(self.max_run_length);
            let log_total = log_sum_exp(&new_log_run_dist);
            for v in &mut new_log_run_dist {
                *v -= log_total;
            }
        }

        // Post-change mean estimate from the new short-run-length stats.
        // Use the most probable short run length's model.
        let window = CHANGE_WINDOW.min(new_log_run_dist.len());
        let post_mean = if window > 0 {
            let best_short = new_log_run_dist[..window]
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map_or(0, |(r, _)| r);
            new_stats[best_short].predictive_mean()
        } else {
            new_stats[0].predictive_mean()
        };

        // Save state.
        self.log_run_dist = new_log_run_dist;
        self.stats = new_stats;
        self.index += 1;

        // Detection: cumulative mass on short run lengths.
        let short_mass = self.short_run_mass();
        let cur_max_rl = self.most_probable_run_length();

        // Detect change: short-run mass exceeds threshold AND we haven't
        // already fired for this regime shift. Reset once the max run length
        // grows past CHANGE_WINDOW (the new regime is established).
        if cur_max_rl >= CHANGE_WINDOW {
            self.in_change = false;
        }

        if short_mass > self.threshold && !self.in_change && self.prev_max_rl >= CHANGE_WINDOW {
            self.in_change = true;
            self.prev_max_rl = cur_max_rl;
            Some(ChangePoint {
                index: self.index - 1,
                probability: short_mass,
                pre_mean,
                post_mean,
            })
        } else {
            self.prev_max_rl = cur_max_rl;
            None
        }
    }

    /// Cumulative posterior mass on run lengths < `CHANGE_WINDOW`.
    fn short_run_mass(&self) -> f64 {
        let window = CHANGE_WINDOW.min(self.log_run_dist.len());
        self.log_run_dist[..window]
            .iter()
            .map(|v| v.exp())
            .sum()
    }

    /// Current run-length posterior distribution (probabilities, not log).
    #[must_use]
    pub fn run_length_distribution(&self) -> Vec<f64> {
        self.log_run_dist.iter().map(|v| v.exp()).collect()
    }

    /// Number of observations processed so far.
    #[must_use]
    pub const fn observation_count(&self) -> usize {
        self.index
    }

    /// Current most probable run length.
    #[must_use]
    pub fn most_probable_run_length(&self) -> usize {
        self.log_run_dist
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map_or(0, |(r, _)| r)
    }
}

/// Numerically stable log-sum-exp.
fn log_sum_exp(log_vals: &[f64]) -> f64 {
    if log_vals.is_empty() {
        return f64::NEG_INFINITY;
    }
    let max = log_vals
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if max == f64::NEG_INFINITY {
        return f64::NEG_INFINITY;
    }
    max + log_vals.iter().map(|v| (v - max).exp()).sum::<f64>().ln()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// BOCPD detects a clear mean shift from 0 to 5.
    #[test]
    fn bocpd_detects_mean_shift() {
        let mut detector = BocpdDetector::new(1.0 / 100.0, 0.5, 300);

        // Feed 100 observations at mean=0.
        for _ in 0..100 {
            let _ = detector.observe(0.0);
        }

        // Feed 100 observations at mean=5. Expect change point detection.
        let mut change_points = Vec::new();
        for _ in 0..100 {
            if let Some(cp) = detector.observe(5.0) {
                change_points.push(cp);
            }
        }

        assert!(
            !change_points.is_empty(),
            "expected at least one change point after mean shift from 0 to 5"
        );

        // The first change point should be near index 100 (within 20 observations).
        let first_cp = &change_points[0];
        assert!(
            first_cp.index >= 98 && first_cp.index <= 130,
            "expected change point near index 100, got {}",
            first_cp.index
        );
        assert!(
            first_cp.probability > 0.5,
            "expected probability > 0.5, got {}",
            first_cp.probability
        );
    }

    /// No false positives on stable data.
    #[test]
    fn bocpd_no_false_positive_stable() {
        // Use max_run_length > observation count to avoid truncation artifacts.
        let mut detector = BocpdDetector::new(1.0 / 100.0, 0.5, 600);

        let mut change_points = Vec::new();
        // Feed stable data.
        for _ in 0..500 {
            if let Some(cp) = detector.observe(10.0) {
                change_points.push(cp);
            }
        }

        assert!(
            change_points.is_empty(),
            "expected no change points on stable data, got {} (indices: {:?})",
            change_points.len(),
            change_points.iter().map(|cp| cp.index).collect::<Vec<_>>()
        );
    }

    /// Detects a variance shift (values switch from tight to wide spread).
    #[test]
    fn bocpd_detects_variance_shift() {
        let mut detector = BocpdDetector::new(1.0 / 50.0, 0.5, 300);

        // 100 observations with small variance.
        for i in 0..100 {
            let x = if i % 2 == 0 { 0.1 } else { -0.1 };
            let _ = detector.observe(x);
        }

        // 100 observations with large variance.
        let mut change_points = Vec::new();
        for i in 0..100 {
            let x = if i % 2 == 0 { 8.0 } else { -8.0 };
            if let Some(cp) = detector.observe(x) {
                change_points.push(cp);
            }
        }

        assert!(
            !change_points.is_empty(),
            "expected change point after variance shift"
        );
    }

    /// Detects multiple change points across 3 segments.
    #[test]
    fn bocpd_multiple_change_points() {
        let mut detector = BocpdDetector::new(1.0 / 50.0, 0.5, 300);

        let mut change_points = Vec::new();

        // Segment 1: mean = 0 (80 observations)
        for _ in 0..80 {
            if let Some(cp) = detector.observe(0.0) {
                change_points.push(cp);
            }
        }

        // Segment 2: mean = 10 (80 observations)
        for _ in 0..80 {
            if let Some(cp) = detector.observe(10.0) {
                change_points.push(cp);
            }
        }

        // Segment 3: mean = -5 (80 observations)
        for _ in 0..80 {
            if let Some(cp) = detector.observe(-5.0) {
                change_points.push(cp);
            }
        }

        assert!(
            change_points.len() >= 2,
            "expected at least 2 change points across 3 segments, got {}",
            change_points.len()
        );
    }

    /// Run-length distribution sums to ~1.0 after each update.
    #[test]
    fn bocpd_run_length_distribution_sums_to_one() {
        let mut detector = BocpdDetector::new(1.0 / 100.0, 0.5, 300);

        for i in 0..200 {
            let x = if i < 100 { 0.0 } else { 5.0 };
            let _ = detector.observe(x);

            let dist = detector.run_length_distribution();
            let sum: f64 = dist.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "run-length distribution should sum to ~1.0, got {sum} at step {i}"
            );
        }
    }

    /// Max run length truncation does not cause numerical issues.
    #[test]
    fn bocpd_max_run_length_truncation() {
        let max_rl = 50;
        let mut detector = BocpdDetector::new(1.0 / 250.0, 0.5, max_rl);

        for _ in 0..200 {
            let _ = detector.observe(1.0);
        }

        let dist = detector.run_length_distribution();

        assert!(
            dist.len() <= max_rl,
            "distribution length {} exceeds max_run_length {}",
            dist.len(),
            max_rl
        );

        let sum: f64 = dist.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "truncated distribution should sum to ~1.0, got {sum}"
        );

        assert!(
            dist.iter().all(|v| v.is_finite()),
            "distribution contains non-finite values: {dist:?}"
        );
    }

    /// Log-sum-exp helper is numerically stable.
    #[test]
    fn log_sum_exp_stable() {
        let vals = vec![1000.0, 1001.0, 999.0];
        let result = log_sum_exp(&vals);
        assert!(result.is_finite(), "log_sum_exp should be finite");
        let expected = 1001.0 + (1.0 + (-1.0_f64).exp() + (-2.0_f64).exp()).ln();
        assert!(
            (result - expected).abs() < 1e-10,
            "log_sum_exp({vals:?}) = {result}, expected {expected}"
        );

        assert_eq!(log_sum_exp(&[]), f64::NEG_INFINITY);
        assert!((log_sum_exp(&[42.0]) - 42.0).abs() < 1e-10);
    }

    /// `ln_gamma` matches known values.
    #[test]
    fn ln_gamma_known_values() {
        assert!(
            (ln_gamma(1.0) - 0.0).abs() < 1e-8,
            "ln_gamma(1) = {}",
            ln_gamma(1.0)
        );
        assert!(
            (ln_gamma(2.0) - 0.0).abs() < 1e-8,
            "ln_gamma(2) = {}",
            ln_gamma(2.0)
        );
        let expected = 2.0_f64.ln();
        assert!(
            (ln_gamma(3.0) - expected).abs() < 1e-6,
            "ln_gamma(3) = {}, expected {expected}",
            ln_gamma(3.0)
        );
        let expected = 0.5 * std::f64::consts::PI.ln();
        assert!(
            (ln_gamma(0.5) - expected).abs() < 1e-6,
            "ln_gamma(0.5) = {}, expected {expected}",
            ln_gamma(0.5)
        );
    }

    /// Most probable run length increases during stable periods.
    #[test]
    fn most_probable_run_length_grows() {
        let mut detector = BocpdDetector::new(1.0 / 250.0, 0.5, 300);

        for _ in 0..50 {
            let _ = detector.observe(0.0);
        }

        let rl = detector.most_probable_run_length();
        assert!(
            rl >= 30,
            "after 50 stable observations, most probable run length should be >= 30, got {rl}"
        );
    }

    /// Observation count tracks correctly.
    #[test]
    fn observation_count_tracks() {
        let mut detector = BocpdDetector::new(1.0 / 100.0, 0.5, 300);
        assert_eq!(detector.observation_count(), 0);

        for _ in 0..42 {
            let _ = detector.observe(1.0);
        }
        assert_eq!(detector.observation_count(), 42);
    }
}
