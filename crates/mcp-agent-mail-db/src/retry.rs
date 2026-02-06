//! Exponential backoff + circuit breaker for `SQLite` lock contention.
//!
//! Matches the legacy Python `retry_on_db_lock` decorator and circuit breaker
//! from `mcp_agent_mail/db.py`.
//!
//! # Backoff Schedule (defaults)
//!
//! | Attempt | Delay (base) | With ±25% jitter |
//! |---------|-------------|------------------|
//! | 0       | 50ms        | 37–63ms          |
//! | 1       | 100ms       | 75–125ms         |
//! | 2       | 200ms       | 150–250ms        |
//! | 3       | 400ms       | 300–500ms        |
//! | 4       | 800ms       | 600–1000ms       |
//! | 5       | 1600ms      | 1200–2000ms      |
//! | 6       | 3200ms      | 2400–4000ms      |
//!
//! # Circuit Breaker
//!
//! After 5 consecutive lock failures the circuit opens for 30 s, failing
//! fast with `CircuitBreakerOpen`. A successful operation after the reset
//! window closes the circuit.

use crate::error::{DbError, DbResult};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Circuit breaker states (matches legacy `CircuitState` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — all calls pass through.
    Closed,
    /// Failing fast — calls are rejected immediately.
    Open,
    /// Testing recovery — one probe call is allowed.
    HalfOpen,
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Open => write!(f, "open"),
            Self::HalfOpen => write!(f, "half_open"),
        }
    }
}

/// Thread-safe circuit breaker for database operations.
///
/// Uses atomics for lock-free reads of state.
pub struct CircuitBreaker {
    /// Consecutive failure count.
    failures: AtomicU32,
    /// Monotonic microseconds when the circuit should close (0 = not open).
    open_until_us: AtomicU64,
    /// Threshold before the circuit opens.
    threshold: u32,
    /// Duration the circuit stays open before entering half-open.
    reset_duration: Duration,
    /// Anchor instant for monotonic time.
    epoch: Instant,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with legacy defaults.
    ///
    /// - `threshold`: 5 consecutive failures before opening
    /// - `reset_duration`: 30 s before half-open
    #[must_use]
    pub fn new() -> Self {
        Self {
            failures: AtomicU32::new(0),
            open_until_us: AtomicU64::new(0),
            threshold: 5,
            reset_duration: Duration::from_secs(30),
            epoch: Instant::now(),
        }
    }

    /// Create with custom parameters.
    #[must_use]
    pub fn with_params(threshold: u32, reset_duration: Duration) -> Self {
        Self {
            failures: AtomicU32::new(0),
            open_until_us: AtomicU64::new(0),
            threshold,
            reset_duration,
            epoch: Instant::now(),
        }
    }

    /// Current circuit state (lock-free read).
    #[must_use]
    pub fn state(&self) -> CircuitState {
        let open_until = self.open_until_us.load(Ordering::Acquire);
        let now_us = self.now_us();

        if open_until > 0 && now_us < open_until {
            return CircuitState::Open;
        }
        if self.failures.load(Ordering::Acquire) >= self.threshold {
            return CircuitState::HalfOpen;
        }
        CircuitState::Closed
    }

    /// Number of consecutive failures.
    #[must_use]
    pub fn failure_count(&self) -> u32 {
        self.failures.load(Ordering::Acquire)
    }

    /// Seconds remaining until the circuit transitions from `Open` to `HalfOpen`.
    /// Returns 0.0 if not open.
    #[must_use]
    pub fn remaining_open_secs(&self) -> f64 {
        let open_until = self.open_until_us.load(Ordering::Acquire);
        if open_until == 0 {
            return 0.0;
        }
        let now_us = self.now_us();
        if now_us >= open_until {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let secs = (open_until - now_us) as f64 / 1_000_000.0;
        secs
    }

    /// Check if a call should be allowed.
    ///
    /// Returns `Ok(())` if the circuit is closed or half-open (probe allowed),
    /// or `Err(CircuitBreakerOpen)` if the circuit is open.
    pub fn check(&self) -> DbResult<()> {
        match self.state() {
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
            CircuitState::Open => Err(DbError::CircuitBreakerOpen {
                message: format!(
                    "Circuit breaker open after {} consecutive failures. \
                     Resets in {:.1}s. Consider: (1) reducing concurrent operations, \
                     (2) increasing busy_timeout, (3) checking for long-running transactions, \
                     (4) running PRAGMA wal_checkpoint(TRUNCATE).",
                    self.failures.load(Ordering::Acquire),
                    self.remaining_open_secs(),
                ),
                failures: self.failures.load(Ordering::Acquire),
                reset_after_secs: self.remaining_open_secs(),
            }),
        }
    }

    /// Record a successful operation — resets the circuit to `Closed`.
    pub fn record_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.open_until_us.store(0, Ordering::Release);
    }

    /// Record a failed operation — may open the circuit.
    pub fn record_failure(&self) {
        let prev = self.failures.fetch_add(1, Ordering::AcqRel);
        let new_count = prev + 1;
        if new_count >= self.threshold {
            let reset_us = micros_from_duration(self.reset_duration);
            let open_until = self.now_us() + reset_us;
            self.open_until_us.store(open_until, Ordering::Release);
        }
    }

    /// Reset the circuit breaker to `Closed` state (for testing or manual recovery).
    pub fn reset(&self) {
        self.failures.store(0, Ordering::Release);
        self.open_until_us.store(0, Ordering::Release);
    }

    fn now_us(&self) -> u64 {
        micros_from_duration(self.epoch.elapsed())
    }
}

/// Convert a [`Duration`] to microseconds as `u64`, saturating on overflow.
#[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
const fn micros_from_duration(d: Duration) -> u64 {
    let us = d.as_micros();
    if us > u64::MAX as u128 {
        u64::MAX
    } else {
        us as u64
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global circuit breaker singleton
// ---------------------------------------------------------------------------

/// Global circuit breaker instance (legacy Python uses module-level globals).
pub static CIRCUIT_BREAKER: std::sync::LazyLock<CircuitBreaker> =
    std::sync::LazyLock::new(CircuitBreaker::new);

// ---------------------------------------------------------------------------
// Retry configuration
// ---------------------------------------------------------------------------

/// Configuration for the exponential backoff retry loop.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (default: 7).
    pub max_retries: u32,
    /// Base delay for the first retry (default: 50ms).
    pub base_delay: Duration,
    /// Maximum delay cap (default: 8s).
    pub max_delay: Duration,
    /// Whether to consult the circuit breaker (default: true).
    pub use_circuit_breaker: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: true,
        }
    }
}

impl RetryConfig {
    /// Calculate the delay for a given attempt (0-indexed).
    ///
    /// Formula: `min(base_delay * 2^attempt, max_delay)` + ±25% jitter.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base_ms = self.base_delay.as_millis() as f64;
        let max_ms = self.max_delay.as_millis() as f64;
        #[allow(clippy::cast_possible_wrap)]
        let exponent = attempt as i32;
        let raw = base_ms.mul_add(2.0_f64.powi(exponent), 0.0).min(max_ms);

        // ±25% jitter to prevent thundering herd.
        let jitter = jitter_factor();
        let jittered = raw.mul_add(0.25 * jitter, raw); // raw * (1 + 0.25*jitter)
        let clamped = jittered.max(10.0); // minimum 10ms

        // Convert to u64 ms, clamping negative to 10.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = clamped.max(0.0) as u64;
        Duration::from_millis(ms)
    }
}

/// Generate a jitter factor in `[-1.0, 1.0]` using a simple LCG.
///
/// We avoid pulling in `rand` — this only needs to break synchronization,
/// not be cryptographically random.
fn jitter_factor() -> f64 {
    use std::sync::atomic::AtomicU64;
    static SEED: AtomicU64 = AtomicU64::new(0);

    // Mix in current time on first use.
    let prev = SEED.load(Ordering::Relaxed);
    if prev == 0 {
        let init = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(12345, |d| {
                let ns = d.as_nanos();
                if ns > u128::from(u64::MAX) {
                    u64::MAX
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = ns as u64;
                    v
                }
            });
        SEED.compare_exchange(0, init, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
    }

    // LCG: x' = (a*x + c) mod 2^64
    let a: u64 = 6_364_136_223_846_793_005;
    let c: u64 = 1_442_695_040_888_963_407;
    let old = SEED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
        Some(x.wrapping_mul(a).wrapping_add(c))
    });
    let val = old.unwrap_or(42);

    // Map to [-1.0, 1.0]
    #[allow(clippy::cast_precision_loss)]
    let mapped = (val as f64 / u64::MAX as f64).mul_add(2.0, -1.0);
    mapped
}

// ---------------------------------------------------------------------------
// Retry wrapper (sync — for use with `std::thread::sleep`)
// ---------------------------------------------------------------------------

/// Execute `op` with exponential backoff retries on lock/busy errors.
///
/// This is a synchronous retry loop using `std::thread::sleep` for backoff.
/// Suitable for wrapping individual DB operations in non-async contexts
/// (e.g., CLI, tests, connection init).
///
/// # Errors
///
/// Returns the last error if all retries are exhausted, or a
/// `CircuitBreakerOpen` error if the circuit is open.
pub fn retry_sync<T, F>(config: &RetryConfig, mut op: F) -> DbResult<T>
where
    F: FnMut() -> DbResult<T>,
{
    let cb = if config.use_circuit_breaker {
        Some(&*CIRCUIT_BREAKER)
    } else {
        None
    };

    let mut last_err = None;

    for attempt in 0..=config.max_retries {
        // Check circuit breaker before each attempt.
        if let Some(cb) = cb {
            cb.check()?;
        }

        match op() {
            Ok(val) => {
                if let Some(cb) = cb {
                    if attempt > 0 {
                        // Successful retry — reset circuit breaker.
                        cb.record_success();
                    }
                }
                return Ok(val);
            }
            Err(e) => {
                if !e.is_retryable() || attempt == config.max_retries {
                    if let Some(cb) = cb {
                        if e.is_retryable() {
                            cb.record_failure();
                        }
                    }
                    return Err(e);
                }

                // Record failure for circuit breaker.
                if let Some(cb) = cb {
                    cb.record_failure();
                }

                last_err = Some(e);

                // Backoff sleep.
                let delay = config.delay_for_attempt(attempt);
                std::thread::sleep(delay);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| DbError::Internal("retry loop exhausted".to_string())))
}

// ---------------------------------------------------------------------------
// Health status
// ---------------------------------------------------------------------------

/// Database health status snapshot (matches legacy `get_db_health_status()`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbHealthStatus {
    /// Current circuit state: "closed", "open", or "`half_open`".
    pub circuit_state: String,
    /// Number of consecutive failures.
    pub circuit_failures: u32,
    /// Recommendation text when circuit is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

/// Return the current database health status.
#[must_use]
pub fn db_health_status() -> DbHealthStatus {
    let cb = &*CIRCUIT_BREAKER;
    let state = cb.state();
    let failures = cb.failure_count();

    let recommendation = if state == CircuitState::Open {
        Some(
            "Circuit breaker is OPEN. Database is experiencing sustained lock contention. \
             Consider: (1) reducing concurrent operations, (2) increasing busy_timeout, \
             (3) checking for long-running transactions, (4) running PRAGMA wal_checkpoint(TRUNCATE)."
                .to_string(),
        )
    } else {
        None
    };

    DbHealthStatus {
        circuit_state: state.to_string(),
        circuit_failures: failures,
        recommendation,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use crate::error::{is_lock_error, is_pool_exhausted_error};

    // -- CircuitBreaker tests -----------------------------------------------

    #[test]
    fn circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::new();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn circuit_breaker_stays_closed_under_threshold() {
        let cb = CircuitBreaker::new();
        for _ in 0..4 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 4);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn circuit_breaker_opens_at_threshold() {
        let cb = CircuitBreaker::with_params(5, Duration::from_secs(30));
        for _ in 0..5 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.failure_count(), 5);
        let err = cb.check().unwrap_err();
        assert!(matches!(err, DbError::CircuitBreakerOpen { .. }));
    }

    #[test]
    fn circuit_breaker_transitions_to_half_open() {
        // Use a very short reset duration so we can test the transition.
        let cb = CircuitBreaker::with_params(3, Duration::from_millis(50));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for reset window to expire.
        std::thread::sleep(Duration::from_millis(70));

        // Should be half-open now (failures still >= threshold but open_until expired).
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        // Half-open allows a probe call.
        assert!(cb.check().is_ok());
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::with_params(3, Duration::from_millis(50));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for half-open.
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Successful probe resets to closed.
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn circuit_breaker_manual_reset() {
        let cb = CircuitBreaker::new();
        for _ in 0..10 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        cb.reset();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
    }

    // -- RetryConfig tests --------------------------------------------------

    #[test]
    fn backoff_schedule_matches_legacy() {
        let config = RetryConfig {
            max_retries: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: false,
        };

        // Expected base delays (before jitter): 50, 100, 200, 400, 800, 1600, 3200
        let expected_base: [i32; 7] = [50, 100, 200, 400, 800, 1600, 3200];
        for (attempt, &expected_ms) in expected_base.iter().enumerate() {
            let delay = config.delay_for_attempt(attempt as u32);
            let ms = delay.as_millis() as f64;
            let base = f64::from(expected_ms);
            let lower = base.mul_add(0.75, -1.0); // -25% jitter + rounding
            let upper = base.mul_add(1.25, 1.0); // +25% jitter + rounding
            assert!(
                ms >= lower && ms <= upper,
                "attempt {attempt}: delay {ms}ms not in [{lower}, {upper}]"
            );
        }
    }

    #[test]
    fn backoff_capped_at_max_delay() {
        let config = RetryConfig {
            max_retries: 20,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: false,
        };

        // Very high attempt should be capped at max_delay.
        let delay = config.delay_for_attempt(15);
        // 8000ms * 1.25 = 10000ms max with jitter
        assert!(delay.as_millis() <= 10_001);
    }

    // -- Error detection tests ----------------------------------------------

    #[test]
    fn lock_error_detection() {
        assert!(is_lock_error("database is locked"));
        assert!(is_lock_error("Database is busy"));
        assert!(is_lock_error("file is locked by another process"));
        assert!(is_lock_error("unable to open database file"));
        assert!(is_lock_error("disk I/O error"));
        assert!(!is_lock_error("syntax error in SQL"));
        assert!(!is_lock_error("table not found"));
    }

    #[test]
    fn pool_exhausted_detection() {
        assert!(is_pool_exhausted_error("pool timeout exceeded"));
        assert!(is_pool_exhausted_error("QueuePool exhausted"));
        assert!(is_pool_exhausted_error("connection pool exhausted"));
        assert!(!is_pool_exhausted_error("database is locked"));
        assert!(!is_pool_exhausted_error("syntax error"));
    }

    #[test]
    fn db_error_is_retryable() {
        assert!(DbError::Sqlite("database is locked".to_string()).is_retryable());
        assert!(DbError::ResourceBusy("locked".to_string()).is_retryable());
        assert!(
            DbError::PoolExhausted {
                message: "timeout".to_string(),
                pool_size: 3,
                max_overflow: 4,
            }
            .is_retryable()
        );
        assert!(!DbError::not_found("agent", "test").is_retryable());
        assert!(!DbError::invalid("field", "bad value").is_retryable());
    }

    #[test]
    fn db_error_codes() {
        assert_eq!(
            DbError::PoolExhausted {
                message: "t".into(),
                pool_size: 3,
                max_overflow: 4,
            }
            .error_code(),
            "DATABASE_POOL_EXHAUSTED"
        );
        assert_eq!(
            DbError::ResourceBusy("t".into()).error_code(),
            "RESOURCE_BUSY"
        );
        assert_eq!(DbError::not_found("agent", "x").error_code(), "NOT_FOUND");
    }

    // -- retry_sync tests ---------------------------------------------------

    #[test]
    fn retry_sync_succeeds_first_try() {
        let config = RetryConfig {
            use_circuit_breaker: false,
            ..Default::default()
        };
        let result = retry_sync(&config, || Ok(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn retry_sync_succeeds_after_retries() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_millis(1), // fast for tests
            max_delay: Duration::from_millis(10),
            use_circuit_breaker: false,
        };

        let attempt = std::cell::Cell::new(0u32);
        let result = retry_sync(&config, || {
            let n = attempt.get();
            attempt.set(n + 1);
            if n < 3 {
                Err(DbError::Sqlite("database is locked".to_string()))
            } else {
                Ok("success")
            }
        });
        assert_eq!(result.unwrap(), "success");
        assert_eq!(attempt.get(), 4);
    }

    #[test]
    fn retry_sync_exhausts_retries() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            use_circuit_breaker: false,
        };

        let attempt = std::cell::Cell::new(0u32);
        let result: DbResult<()> = retry_sync(&config, || {
            attempt.set(attempt.get() + 1);
            Err(DbError::Sqlite("database is locked".to_string()))
        });
        assert!(result.is_err());
        // max_retries=3 means 4 attempts total (0..=3)
        assert_eq!(attempt.get(), 4);
    }

    #[test]
    fn retry_sync_non_retryable_fails_immediately() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            use_circuit_breaker: false,
        };

        let attempt = std::cell::Cell::new(0u32);
        let result: DbResult<()> = retry_sync(&config, || {
            attempt.set(attempt.get() + 1);
            Err(DbError::not_found("agent", "missing"))
        });
        assert!(result.is_err());
        assert_eq!(attempt.get(), 1); // No retries for non-retryable errors
    }

    #[test]
    fn retry_sync_with_circuit_breaker() {
        let cb = CircuitBreaker::with_params(3, Duration::from_secs(30));

        // Manually open the circuit.
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // A manual check should fail.
        let err = cb.check().unwrap_err();
        assert!(matches!(err, DbError::CircuitBreakerOpen { .. }));
        assert!(err.is_recoverable());
    }

    // -- Health status test -------------------------------------------------

    #[test]
    fn health_status_closed() {
        // Reset global CB for this test.
        CIRCUIT_BREAKER.reset();
        let status = db_health_status();
        assert_eq!(status.circuit_state, "closed");
        assert_eq!(status.circuit_failures, 0);
        assert!(status.recommendation.is_none());
    }

    // -- Jitter test --------------------------------------------------------

    #[test]
    fn jitter_produces_varied_values() {
        let mut values = Vec::new();
        for _ in 0..20 {
            values.push(jitter_factor());
        }
        // At minimum, not all values should be identical.
        let first = values[0];
        let has_variation = values.iter().any(|v| (v - first).abs() > 0.01);
        assert!(
            has_variation,
            "jitter should produce varied values: {values:?}"
        );
    }

    // -- Legacy parity tests ------------------------------------------------

    /// Verify retry + circuit breaker defaults match legacy Python `retry_on_db_lock`.
    ///
    /// Python legacy:
    /// - `max_retries`: 7 (attempts 0..=7)
    /// - `base_delay`: 50ms
    /// - `max_delay`: 8s
    /// - jitter: ±25%
    /// - circuit threshold: 5 consecutive failures
    /// - circuit reset: 30s
    #[test]
    fn retry_defaults_match_legacy_python() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 7, "legacy max_retries is 7");
        assert_eq!(
            config.base_delay,
            Duration::from_millis(50),
            "legacy base_delay is 50ms"
        );
        assert_eq!(
            config.max_delay,
            Duration::from_secs(8),
            "legacy max_delay is 8s"
        );
        assert!(
            config.use_circuit_breaker,
            "circuit breaker should be enabled by default"
        );

        let cb = CircuitBreaker::new();
        assert_eq!(cb.threshold, 5, "legacy circuit threshold is 5");
        assert_eq!(
            cb.reset_duration,
            Duration::from_secs(30),
            "legacy circuit reset is 30s"
        );
    }
}
