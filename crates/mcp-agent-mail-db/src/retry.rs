//! Exponential backoff + per-subsystem circuit breakers.
//!
//! Matches the legacy Python `retry_on_db_lock` decorator and circuit breaker
//! from `mcp_agent_mail/db.py`, extended with per-subsystem isolation so that
//! a git failure cannot take down database operations (and vice versa).
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
//! # Per-Subsystem Circuit Breakers
//!
//! Each subsystem (DB, Git, Signal, LLM) has an independent circuit breaker
//! so that failures in one do not cascade to others. Each circuit has:
//! - Independent threshold and reset duration (configurable via env vars)
//! - Rate-limited half-open probes (max 1 per 5 s)
//! - Success threshold: 3 consecutive successes required to close from half-open
//! - WARN-level logging on state transitions

use crate::error::{DbError, DbResult};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Subsystem identification
// ---------------------------------------------------------------------------

/// Identifies which subsystem a circuit breaker protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subsystem {
    /// `SQLite` operations (pool acquire, query execution).
    Db,
    /// Git archive operations (commit, read).
    Git,
    /// Notification signal writes.
    Signal,
    /// LLM API calls.
    Llm,
}

impl Subsystem {
    /// All subsystem variants for iteration.
    pub const ALL: [Self; 4] = [Self::Db, Self::Git, Self::Signal, Self::Llm];
}

impl std::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db => write!(f, "db"),
            Self::Git => write!(f, "git"),
            Self::Signal => write!(f, "signal"),
            Self::Llm => write!(f, "llm"),
        }
    }
}

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
    /// Testing recovery — rate-limited probe calls are allowed.
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

/// Minimum interval between half-open probe attempts (5 seconds).
const HALF_OPEN_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Number of consecutive successes required to close from half-open.
const HALF_OPEN_SUCCESS_THRESHOLD: u32 = 3;

/// Thread-safe circuit breaker with per-subsystem isolation.
///
/// Uses atomics for lock-free reads of state. Enhanced over the legacy
/// single-breaker design with:
/// - Subsystem label for diagnostics
/// - Rate-limited half-open probes (max 1 per `HALF_OPEN_PROBE_INTERVAL`)
/// - Success threshold: requires `HALF_OPEN_SUCCESS_THRESHOLD` consecutive
///   successes in half-open before closing
/// - WARN-level logging on state transitions
pub struct CircuitBreaker {
    /// Consecutive failure count.
    failures: AtomicU32,
    /// Consecutive successes in half-open state (resets on failure or close).
    half_open_successes: AtomicU32,
    /// Monotonic microseconds when the circuit should enter half-open (0 = not open).
    open_until_us: AtomicU64,
    /// Monotonic microseconds of the last half-open probe (0 = never).
    last_probe_us: AtomicU64,
    /// Threshold before the circuit opens.
    threshold: u32,
    /// Duration the circuit stays open before entering half-open.
    reset_duration: Duration,
    /// Anchor instant for monotonic time.
    epoch: Instant,
    /// Subsystem this breaker protects (for logging and diagnostics).
    subsystem: &'static str,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with legacy defaults.
    ///
    /// - `threshold`: 5 consecutive failures before opening
    /// - `reset_duration`: 30 s before half-open
    #[must_use]
    pub fn new() -> Self {
        Self::with_subsystem("db", 5, Duration::from_secs(30))
    }

    /// Create with custom parameters (legacy API).
    #[must_use]
    pub fn with_params(threshold: u32, reset_duration: Duration) -> Self {
        Self::with_subsystem("db", threshold, reset_duration)
    }

    /// Create with subsystem label and custom parameters.
    #[must_use]
    pub fn with_subsystem(
        subsystem: &'static str,
        threshold: u32,
        reset_duration: Duration,
    ) -> Self {
        Self {
            failures: AtomicU32::new(0),
            half_open_successes: AtomicU32::new(0),
            open_until_us: AtomicU64::new(0),
            last_probe_us: AtomicU64::new(0),
            threshold,
            reset_duration,
            epoch: Instant::now(),
            subsystem,
        }
    }

    /// The subsystem label this breaker protects.
    #[must_use]
    pub const fn subsystem(&self) -> &str {
        self.subsystem
    }

    /// The configured failure threshold.
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    /// The configured reset duration.
    #[must_use]
    pub const fn reset_duration(&self) -> Duration {
        self.reset_duration
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

    /// Number of consecutive half-open successes (toward the close threshold).
    #[must_use]
    pub fn half_open_success_count(&self) -> u32 {
        self.half_open_successes.load(Ordering::Acquire)
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
    /// Returns `Ok(())` if the circuit is closed, or if the circuit is half-open
    /// and the rate-limited probe interval has elapsed.
    /// Returns `Err(CircuitBreakerOpen)` if the circuit is open, or if we're
    /// in half-open but a probe was already issued recently.
    pub fn check(&self) -> DbResult<()> {
        match self.state() {
            CircuitState::Closed => Ok(()),
            CircuitState::HalfOpen => {
                // Rate-limit probes: only allow one per HALF_OPEN_PROBE_INTERVAL.
                let now_us = self.now_us();
                let last = self.last_probe_us.load(Ordering::Acquire);
                let interval_us = micros_from_duration(HALF_OPEN_PROBE_INTERVAL);
                if last > 0 && now_us.saturating_sub(last) < interval_us {
                    #[allow(clippy::cast_precision_loss)]
                    let remaining =
                        (interval_us - now_us.saturating_sub(last)) as f64 / 1_000_000.0;
                    return Err(DbError::CircuitBreakerOpen {
                        message: format!(
                            "[{subsystem}] Circuit breaker half-open, probe rate-limited. \
                             Next probe in {remaining:.1}s.",
                            subsystem = self.subsystem,
                        ),
                        failures: self.failures.load(Ordering::Acquire),
                        reset_after_secs: 0.0,
                    });
                }
                // Allow probe and record the time.
                self.last_probe_us.store(now_us, Ordering::Release);
                Ok(())
            }
            CircuitState::Open => Err(DbError::CircuitBreakerOpen {
                message: format!(
                    "[{subsystem}] Circuit breaker open after {failures} consecutive failures. \
                     Resets in {remaining:.1}s.",
                    subsystem = self.subsystem,
                    failures = self.failures.load(Ordering::Acquire),
                    remaining = self.remaining_open_secs(),
                ),
                failures: self.failures.load(Ordering::Acquire),
                reset_after_secs: self.remaining_open_secs(),
            }),
        }
    }

    /// Record a successful operation.
    ///
    /// In half-open state, increments the success counter. Once
    /// `HALF_OPEN_SUCCESS_THRESHOLD` consecutive successes are reached,
    /// the circuit closes. In closed state, resets any stale failure count.
    pub fn record_success(&self) {
        let prev_state = self.state();
        match prev_state {
            CircuitState::HalfOpen => {
                let prev = self.half_open_successes.fetch_add(1, Ordering::AcqRel);
                if prev + 1 >= HALF_OPEN_SUCCESS_THRESHOLD {
                    // Enough consecutive successes — close the circuit.
                    self.failures.store(0, Ordering::Release);
                    self.open_until_us.store(0, Ordering::Release);
                    self.half_open_successes.store(0, Ordering::Release);
                    self.last_probe_us.store(0, Ordering::Release);
                    log_transition(self.subsystem, prev_state, CircuitState::Closed);
                }
            }
            CircuitState::Closed => {
                // Reset stale failures to prevent accumulation across calls.
                self.failures.store(0, Ordering::Release);
            }
            CircuitState::Open => {
                // Shouldn't happen (check() blocks open), but be safe.
                self.failures.store(0, Ordering::Release);
                self.open_until_us.store(0, Ordering::Release);
                self.half_open_successes.store(0, Ordering::Release);
                self.last_probe_us.store(0, Ordering::Release);
            }
        }
    }

    /// Record a failed operation — may open the circuit.
    pub fn record_failure(&self) {
        let prev_state = self.state();
        // Reset half-open success streak on any failure.
        self.half_open_successes.store(0, Ordering::Release);

        let prev = self.failures.fetch_add(1, Ordering::AcqRel);
        let new_count = prev + 1;
        if new_count >= self.threshold {
            let was_already_open = self.open_until_us.load(Ordering::Acquire) > 0
                && self.now_us() < self.open_until_us.load(Ordering::Acquire);
            let reset_us = micros_from_duration(self.reset_duration);
            let open_until = self.now_us() + reset_us;
            self.open_until_us.store(open_until, Ordering::Release);
            let new_state = CircuitState::Open;
            if !was_already_open && prev_state != CircuitState::Open {
                log_transition(self.subsystem, prev_state, new_state);
            }
        }
    }

    /// Reset the circuit breaker to `Closed` state (for testing or manual recovery).
    pub fn reset(&self) {
        self.failures.store(0, Ordering::Release);
        self.open_until_us.store(0, Ordering::Release);
        self.half_open_successes.store(0, Ordering::Release);
        self.last_probe_us.store(0, Ordering::Release);
    }

    fn now_us(&self) -> u64 {
        micros_from_duration(self.epoch.elapsed())
    }
}

/// Log a circuit state transition at WARN level.
fn log_transition(subsystem: &str, from: CircuitState, to: CircuitState) {
    tracing::warn!(
        subsystem,
        %from,
        %to,
        "circuit breaker state transition",
    );
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
// Per-subsystem circuit breaker globals
// ---------------------------------------------------------------------------

/// Read a u32 from an environment variable, returning `default` on missing/parse error.
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Read a u64 from an environment variable, returning `default` on missing/parse error.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Circuit breaker for **database** (`SQLite`) operations.
///
/// Env vars: `CIRCUIT_DB_THRESHOLD` (default 5), `CIRCUIT_DB_RESET_SECS` (default 30).
pub static CIRCUIT_DB: std::sync::LazyLock<CircuitBreaker> = std::sync::LazyLock::new(|| {
    CircuitBreaker::with_subsystem(
        "db",
        env_u32("CIRCUIT_DB_THRESHOLD", 5),
        Duration::from_secs(env_u64("CIRCUIT_DB_RESET_SECS", 30)),
    )
});

/// Circuit breaker for **git** archive operations.
///
/// Git is inherently flakier (index.lock, network), so defaults are more lenient.
/// Env vars: `CIRCUIT_GIT_THRESHOLD` (default 8), `CIRCUIT_GIT_RESET_SECS` (default 45).
pub static CIRCUIT_GIT: std::sync::LazyLock<CircuitBreaker> = std::sync::LazyLock::new(|| {
    CircuitBreaker::with_subsystem(
        "git",
        env_u32("CIRCUIT_GIT_THRESHOLD", 8),
        Duration::from_secs(env_u64("CIRCUIT_GIT_RESET_SECS", 45)),
    )
});

/// Circuit breaker for **signal** (notification) writes.
///
/// Env vars: `CIRCUIT_SIGNAL_THRESHOLD` (default 5), `CIRCUIT_SIGNAL_RESET_SECS` (default 30).
pub static CIRCUIT_SIGNAL: std::sync::LazyLock<CircuitBreaker> = std::sync::LazyLock::new(|| {
    CircuitBreaker::with_subsystem(
        "signal",
        env_u32("CIRCUIT_SIGNAL_THRESHOLD", 5),
        Duration::from_secs(env_u64("CIRCUIT_SIGNAL_RESET_SECS", 30)),
    )
});

/// Circuit breaker for **LLM** API calls.
///
/// LLM depends on external APIs — more lenient threshold, longer reset.
/// Env vars: `CIRCUIT_LLM_THRESHOLD` (default 3), `CIRCUIT_LLM_RESET_SECS` (default 60).
pub static CIRCUIT_LLM: std::sync::LazyLock<CircuitBreaker> = std::sync::LazyLock::new(|| {
    CircuitBreaker::with_subsystem(
        "llm",
        env_u32("CIRCUIT_LLM_THRESHOLD", 3),
        Duration::from_secs(env_u64("CIRCUIT_LLM_RESET_SECS", 60)),
    )
});

/// Legacy global circuit breaker — aliased to `CIRCUIT_DB` for backward compatibility.
///
/// New code should use `CIRCUIT_DB` directly, or the appropriate subsystem breaker.
pub static CIRCUIT_BREAKER: std::sync::LazyLock<CircuitBreaker> =
    std::sync::LazyLock::new(CircuitBreaker::new);

/// Look up the circuit breaker for a given subsystem.
#[must_use]
pub fn circuit_for(subsystem: Subsystem) -> &'static CircuitBreaker {
    match subsystem {
        Subsystem::Db => &CIRCUIT_DB,
        Subsystem::Git => &CIRCUIT_GIT,
        Subsystem::Signal => &CIRCUIT_SIGNAL,
        Subsystem::Llm => &CIRCUIT_LLM,
    }
}

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
    /// Which subsystem circuit breaker to use (default: `Db`).
    pub subsystem: Subsystem,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: true,
            subsystem: Subsystem::Db,
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
        Some(circuit_for(config.subsystem))
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
                    // Reset circuit breaker on any success, including
                    // first attempt. Stale failures must not accumulate
                    // across successful calls.
                    cb.record_success();
                }
                return Ok(val);
            }
            Err(e) => {
                if !e.is_retryable() || attempt == config.max_retries {
                    if let Some(cb) = cb
                        && e.is_retryable()
                    {
                        cb.record_failure();
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

/// Per-subsystem circuit breaker status snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubsystemCircuitStatus {
    /// Subsystem name (e.g. "db", "git", "signal", "llm").
    pub subsystem: String,
    /// Current circuit state: `"closed"`, `"open"`, or `"half_open"`.
    pub state: String,
    /// Number of consecutive failures.
    pub failures: u32,
    /// Configured failure threshold.
    pub threshold: u32,
    /// Configured reset duration in seconds.
    pub reset_secs: u64,
    /// Consecutive successes in half-open (toward close threshold).
    pub half_open_successes: u32,
    /// Recommendation text when circuit is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

/// Database health status snapshot (matches legacy `get_db_health_status()`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbHealthStatus {
    /// Current circuit state: `"closed"`, `"open"`, or `"half_open"`.
    pub circuit_state: String,
    /// Number of consecutive failures.
    pub circuit_failures: u32,
    /// Recommendation text when circuit is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// Per-subsystem circuit breaker statuses.
    pub circuits: Vec<SubsystemCircuitStatus>,
}

/// Return the current database health status including all subsystem circuits.
#[must_use]
pub fn db_health_status() -> DbHealthStatus {
    // Primary DB circuit for backward-compatible top-level fields.
    let db_cb = &*CIRCUIT_DB;
    let db_state = db_cb.state();
    let db_failures = db_cb.failure_count();

    let recommendation = if db_state == CircuitState::Open {
        Some(
            "Circuit breaker [db] is OPEN. Database is experiencing sustained lock contention. \
             Consider: (1) reducing concurrent operations, (2) increasing busy_timeout, \
             (3) checking for long-running transactions, (4) running PRAGMA wal_checkpoint(TRUNCATE)."
                .to_string(),
        )
    } else {
        None
    };

    let circuits = Subsystem::ALL
        .iter()
        .map(|&sub| {
            let cb = circuit_for(sub);
            let state = cb.state();
            let rec = match (sub, state) {
                (Subsystem::Db, CircuitState::Open) => Some(
                    "DB circuit OPEN: reduce concurrent operations or increase busy_timeout."
                        .to_string(),
                ),
                (Subsystem::Git, CircuitState::Open) => Some(
                    "Git circuit OPEN: check for index.lock contention or network issues."
                        .to_string(),
                ),
                (Subsystem::Signal, CircuitState::Open) => Some(
                    "Signal circuit OPEN: check filesystem permissions and disk space.".to_string(),
                ),
                (Subsystem::Llm, CircuitState::Open) => {
                    Some("LLM circuit OPEN: check API keys and network connectivity.".to_string())
                }
                _ => None,
            };
            SubsystemCircuitStatus {
                subsystem: sub.to_string(),
                state: state.to_string(),
                failures: cb.failure_count(),
                threshold: cb.threshold(),
                reset_secs: cb.reset_duration().as_secs(),
                half_open_successes: cb.half_open_success_count(),
                recommendation: rec,
            }
        })
        .collect();

    DbHealthStatus {
        circuit_state: db_state.to_string(),
        circuit_failures: db_failures,
        recommendation,
        circuits,
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
    fn half_open_requires_multiple_successes_to_close() {
        let cb = CircuitBreaker::with_params(3, Duration::from_millis(50));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for half-open.
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // First success — still half-open (need HALF_OPEN_SUCCESS_THRESHOLD).
        cb.record_success();
        assert_eq!(cb.half_open_success_count(), 1);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Second success — still half-open.
        cb.record_success();
        assert_eq!(cb.half_open_success_count(), 2);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Third success — circuit closes.
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
        assert_eq!(cb.half_open_success_count(), 0);
    }

    #[test]
    fn half_open_failure_resets_success_streak() {
        let cb = CircuitBreaker::with_params(3, Duration::from_millis(50));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Two successes, then a failure.
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.half_open_success_count(), 2);

        cb.record_failure();
        assert_eq!(cb.half_open_success_count(), 0);
        // Failure re-opens the circuit.
        assert_eq!(cb.state(), CircuitState::Open);
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

        // 3 successful probes close the circuit.
        for _ in 0..HALF_OPEN_SUCCESS_THRESHOLD {
            cb.record_success();
        }
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
        assert_eq!(cb.half_open_success_count(), 0);
    }

    // -- Subsystem tests ----------------------------------------------------

    #[test]
    fn subsystem_display() {
        assert_eq!(Subsystem::Db.to_string(), "db");
        assert_eq!(Subsystem::Git.to_string(), "git");
        assert_eq!(Subsystem::Signal.to_string(), "signal");
        assert_eq!(Subsystem::Llm.to_string(), "llm");
    }

    #[test]
    fn subsystem_all_contains_four() {
        assert_eq!(Subsystem::ALL.len(), 4);
    }

    #[test]
    fn per_subsystem_circuit_independence() {
        // Open the git circuit — DB should remain closed.
        let git = CircuitBreaker::with_subsystem("git", 2, Duration::from_secs(30));
        let db = CircuitBreaker::with_subsystem("db", 5, Duration::from_secs(30));

        git.record_failure();
        git.record_failure();
        assert_eq!(git.state(), CircuitState::Open);
        assert_eq!(db.state(), CircuitState::Closed);

        // DB operations still allowed.
        assert!(db.check().is_ok());
        // Git operations blocked.
        assert!(git.check().is_err());
    }

    #[test]
    fn subsystem_label_in_error_message() {
        let cb = CircuitBreaker::with_subsystem("git", 2, Duration::from_secs(30));
        cb.record_failure();
        cb.record_failure();
        let err = cb.check().unwrap_err();
        if let DbError::CircuitBreakerOpen { message, .. } = err {
            assert!(
                message.contains("[git]"),
                "error should name subsystem: {message}"
            );
        } else {
            panic!("expected CircuitBreakerOpen");
        }
    }

    #[test]
    fn circuit_for_returns_correct_subsystem() {
        let db = circuit_for(Subsystem::Db);
        assert_eq!(db.subsystem(), "db");
        let git = circuit_for(Subsystem::Git);
        assert_eq!(git.subsystem(), "git");
        let signal = circuit_for(Subsystem::Signal);
        assert_eq!(signal.subsystem(), "signal");
        let llm = circuit_for(Subsystem::Llm);
        assert_eq!(llm.subsystem(), "llm");
    }

    #[test]
    fn global_circuits_have_correct_defaults() {
        // DB: threshold=5, reset=30s
        assert_eq!(CIRCUIT_DB.threshold(), 5);
        assert_eq!(CIRCUIT_DB.reset_duration(), Duration::from_secs(30));
        // Git: threshold=8, reset=45s
        assert_eq!(CIRCUIT_GIT.threshold(), 8);
        assert_eq!(CIRCUIT_GIT.reset_duration(), Duration::from_secs(45));
        // Signal: threshold=5, reset=30s
        assert_eq!(CIRCUIT_SIGNAL.threshold(), 5);
        assert_eq!(CIRCUIT_SIGNAL.reset_duration(), Duration::from_secs(30));
        // LLM: threshold=3, reset=60s
        assert_eq!(CIRCUIT_LLM.threshold(), 3);
        assert_eq!(CIRCUIT_LLM.reset_duration(), Duration::from_mins(1));
    }

    // -- Half-open rate limiting tests ---------------------------------------

    #[test]
    fn half_open_rate_limits_probes() {
        let cb = CircuitBreaker::with_params(2, Duration::from_millis(50));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // First probe: allowed.
        assert!(cb.check().is_ok());

        // Immediate second probe: rate-limited (within 5s interval).
        let err = cb.check().unwrap_err();
        assert!(matches!(err, DbError::CircuitBreakerOpen { .. }));
        if let DbError::CircuitBreakerOpen { message, .. } = err {
            assert!(
                message.contains("rate-limited"),
                "should mention rate limit: {message}"
            );
        }
    }

    // -- RetryConfig tests --------------------------------------------------

    #[test]
    fn retry_config_default_subsystem_is_db() {
        let config = RetryConfig::default();
        assert_eq!(config.subsystem, Subsystem::Db);
    }

    #[test]
    fn backoff_schedule_matches_legacy() {
        let config = RetryConfig {
            max_retries: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: false,
            subsystem: Subsystem::Db,
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
            subsystem: Subsystem::Db,
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    // -- Health status tests ------------------------------------------------
    // These tests manipulate global circuit-breaker statics and must not run
    // in parallel with each other (or with `retry_sync_with_circuit_breaker`
    // which also touches a global CB).  A simple mutex serialises them.

    static HEALTH_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn health_status_closed() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        // Reset global CBs for this test.
        CIRCUIT_BREAKER.reset();
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();
        let status = db_health_status();
        assert_eq!(status.circuit_state, "closed");
        assert_eq!(status.circuit_failures, 0);
        assert!(status.recommendation.is_none());
        assert_eq!(status.circuits.len(), 4);
        for circuit in &status.circuits {
            assert_eq!(circuit.state, "closed");
            assert_eq!(circuit.failures, 0);
            assert!(circuit.recommendation.is_none());
        }
    }

    #[test]
    fn health_status_shows_per_subsystem() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();

        // Open just the git circuit.
        for _ in 0..8 {
            CIRCUIT_GIT.record_failure();
        }
        assert_eq!(CIRCUIT_GIT.state(), CircuitState::Open);

        let status = db_health_status();
        // Top-level should still be "closed" (DB is fine).
        assert_eq!(status.circuit_state, "closed");

        // Git circuit should show as open.
        let git_status = status
            .circuits
            .iter()
            .find(|c| c.subsystem == "git")
            .unwrap();
        assert_eq!(git_status.state, "open");
        assert!(git_status.recommendation.is_some());
        assert!(
            git_status
                .recommendation
                .as_ref()
                .unwrap()
                .contains("index.lock")
        );

        // DB circuit should be closed.
        let db_status = status
            .circuits
            .iter()
            .find(|c| c.subsystem == "db")
            .unwrap();
        assert_eq!(db_status.state, "closed");
        assert!(db_status.recommendation.is_none());

        // Clean up.
        CIRCUIT_GIT.reset();
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
        assert_eq!(config.subsystem, Subsystem::Db, "default subsystem is DB");

        let cb = CircuitBreaker::new();
        assert_eq!(cb.threshold, 5, "legacy circuit threshold is 5");
        assert_eq!(
            cb.reset_duration,
            Duration::from_secs(30),
            "legacy circuit reset is 30s"
        );
    }

    // -- remaining_open_secs tests ------------------------------------------

    #[test]
    fn remaining_open_secs_zero_when_closed() {
        let cb = CircuitBreaker::with_params(3, Duration::from_secs(30));
        assert!(cb.remaining_open_secs() <= f64::EPSILON);
    }

    #[test]
    fn remaining_open_secs_positive_when_open() {
        let cb = CircuitBreaker::with_params(2, Duration::from_secs(10));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        let remaining = cb.remaining_open_secs();
        // Should be close to 10s (some time elapsed since record_failure).
        assert!(
            remaining > 8.0 && remaining <= 10.0,
            "expected ~10s remaining, got {remaining}"
        );
    }

    #[test]
    fn remaining_open_secs_zero_after_expiry() {
        let cb = CircuitBreaker::with_params(2, Duration::from_millis(30));
        cb.record_failure();
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(50));
        assert!(cb.remaining_open_secs() <= f64::EPSILON);
    }

    #[test]
    fn remaining_open_secs_zero_after_reset() {
        let cb = CircuitBreaker::with_params(2, Duration::from_secs(30));
        cb.record_failure();
        cb.record_failure();
        assert!(cb.remaining_open_secs() > 0.0);
        cb.reset();
        assert!(cb.remaining_open_secs() <= f64::EPSILON);
    }

    // -- CircuitState Display tests -----------------------------------------

    #[test]
    fn circuit_state_display() {
        assert_eq!(CircuitState::Closed.to_string(), "closed");
        assert_eq!(CircuitState::Open.to_string(), "open");
        assert_eq!(CircuitState::HalfOpen.to_string(), "half_open");
    }

    // -- record_success in Open state (defensive branch) --------------------

    #[test]
    fn record_success_in_open_state_closes_circuit() {
        let cb = CircuitBreaker::with_params(2, Duration::from_mins(5));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        // Directly call record_success while open (shouldn't happen in practice
        // since check() blocks, but test the defensive branch).
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
        assert_eq!(cb.half_open_success_count(), 0);
    }

    // -- with_params defaults to "db" subsystem label -----------------------

    #[test]
    fn with_params_uses_db_subsystem() {
        let cb = CircuitBreaker::with_params(5, Duration::from_secs(30));
        assert_eq!(cb.subsystem(), "db");
    }

    #[test]
    fn default_circuit_breaker_uses_db_subsystem() {
        let cb = CircuitBreaker::default();
        assert_eq!(cb.subsystem(), "db");
        assert_eq!(cb.threshold(), 5);
        assert_eq!(cb.reset_duration(), Duration::from_secs(30));
    }

    // -- Jitter range validation --------------------------------------------

    #[test]
    fn jitter_factor_stays_in_range() {
        for _ in 0..100 {
            let j = jitter_factor();
            assert!(
                (-1.0..=1.0).contains(&j),
                "jitter_factor out of [-1.0, 1.0]: {j}"
            );
        }
    }

    // -- delay_for_attempt minimum floor ------------------------------------

    #[test]
    fn delay_for_attempt_has_minimum_floor() {
        let config = RetryConfig {
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_secs(1),
            use_circuit_breaker: false,
            ..Default::default()
        };
        // Even with very small base_delay, should be at least 10ms.
        for attempt in 0..5 {
            let delay = config.delay_for_attempt(attempt);
            assert!(
                delay.as_millis() >= 10,
                "attempt {attempt}: delay {}ms below 10ms floor",
                delay.as_millis()
            );
        }
    }

    // -- retry_sync with circuit breaker blocking ---------------------------

    #[test]
    fn retry_sync_blocked_by_open_circuit_breaker() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        // Open the DB circuit.
        CIRCUIT_DB.reset();
        for _ in 0..5 {
            CIRCUIT_DB.record_failure();
        }
        assert_eq!(CIRCUIT_DB.state(), CircuitState::Open);

        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            use_circuit_breaker: true,
            subsystem: Subsystem::Db,
        };

        let attempt = std::cell::Cell::new(0u32);
        let result: DbResult<()> = retry_sync(&config, || {
            attempt.set(attempt.get() + 1);
            Ok(())
        });

        // Should fail without calling op (circuit breaker blocks before first attempt).
        assert!(result.is_err());
        assert_eq!(attempt.get(), 0, "op should not be called when CB is open");
        let err = result.unwrap_err();
        assert!(matches!(err, DbError::CircuitBreakerOpen { .. }));

        // Clean up.
        CIRCUIT_DB.reset();
    }

    // -- Health status serialization ----------------------------------------

    #[test]
    fn health_status_serializes_to_json() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();

        let status = db_health_status();
        let json = serde_json::to_value(&status).expect("serialize DbHealthStatus");

        assert_eq!(json["circuit_state"], "closed");
        assert_eq!(json["circuit_failures"], 0);
        // recommendation should be absent (skip_serializing_if = None).
        assert!(json.get("recommendation").is_none());

        let circuits = json["circuits"].as_array().expect("circuits array");
        assert_eq!(circuits.len(), 4);
        for circuit in circuits {
            assert!(circuit["subsystem"].is_string());
            assert_eq!(circuit["state"], "closed");
            assert_eq!(circuit["failures"], 0);
            assert!(circuit["threshold"].is_number());
            assert!(circuit["reset_secs"].is_number());
            assert_eq!(circuit["half_open_successes"], 0);
            // No recommendation when closed.
            assert!(circuit.get("recommendation").is_none());
        }
    }

    #[test]
    fn health_status_open_circuit_has_recommendation() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();

        // Open DB circuit.
        for _ in 0..5 {
            CIRCUIT_DB.record_failure();
        }

        let status = db_health_status();
        assert_eq!(status.circuit_state, "open");
        assert!(status.recommendation.is_some());
        assert!(status.recommendation.as_ref().unwrap().contains("OPEN"));

        let json = serde_json::to_value(&status).expect("serialize");
        assert!(json["recommendation"].is_string());

        // Verify per-subsystem recommendations.
        let db_circuit = json["circuits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["subsystem"] == "db")
            .unwrap();
        assert!(db_circuit["recommendation"].is_string());

        // Other circuits should have no recommendation.
        let git_circuit = json["circuits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["subsystem"] == "git")
            .unwrap();
        assert!(git_circuit.get("recommendation").is_none());

        CIRCUIT_DB.reset();
    }

    // -- Subsystem::ALL completeness ----------------------------------------

    #[test]
    fn subsystem_all_covers_every_variant() {
        // Ensure ALL contains exactly the variants we expect.
        let all_set: std::collections::HashSet<Subsystem> =
            Subsystem::ALL.iter().copied().collect();
        assert!(all_set.contains(&Subsystem::Db));
        assert!(all_set.contains(&Subsystem::Git));
        assert!(all_set.contains(&Subsystem::Signal));
        assert!(all_set.contains(&Subsystem::Llm));
        assert_eq!(all_set.len(), 4);
    }

    // -- Per-subsystem recommendation strings -------------------------------

    #[test]
    fn each_open_subsystem_has_recommendation() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();

        let subsystem_info: [(Subsystem, u32, &str); 4] = [
            (Subsystem::Db, 5, "busy_timeout"),
            (Subsystem::Git, 8, "index.lock"),
            (Subsystem::Signal, 5, "filesystem"),
            (Subsystem::Llm, 3, "API keys"),
        ];

        for (sub, threshold, _keyword) in &subsystem_info {
            let cb = circuit_for(*sub);
            for _ in 0..*threshold {
                cb.record_failure();
            }
            assert_eq!(
                cb.state(),
                CircuitState::Open,
                "{sub} should be open after {threshold} failures"
            );
        }

        let status = db_health_status();
        for (sub, _, expected_keyword) in &subsystem_info {
            let circuit_status = status
                .circuits
                .iter()
                .find(|c| c.subsystem == sub.to_string())
                .unwrap_or_else(|| panic!("missing circuit for {sub}"));
            assert_eq!(circuit_status.state, "open", "{sub} should be open");
            let rec = circuit_status
                .recommendation
                .as_ref()
                .unwrap_or_else(|| panic!("{sub}: expected recommendation"));
            assert!(
                rec.contains(expected_keyword),
                "{sub}: recommendation should contain '{expected_keyword}', got: {rec}"
            );
        }

        // Clean up.
        CIRCUIT_DB.reset();
        CIRCUIT_GIT.reset();
        CIRCUIT_SIGNAL.reset();
        CIRCUIT_LLM.reset();
    }

    // -- micros_from_duration edge cases ------------------------------------

    #[test]
    fn micros_from_duration_normal() {
        assert_eq!(micros_from_duration(Duration::from_secs(1)), 1_000_000);
        assert_eq!(micros_from_duration(Duration::from_millis(500)), 500_000);
        assert_eq!(micros_from_duration(Duration::ZERO), 0);
    }

    #[test]
    fn micros_from_duration_saturates_on_huge_duration() {
        // Duration::MAX is enormous; micros_from_duration should saturate to u64::MAX.
        let huge = Duration::new(u64::MAX, 999_999_999);
        assert_eq!(micros_from_duration(huge), u64::MAX);
    }

    // -- retry_sync records failures to circuit breaker ---------------------

    #[test]
    fn retry_sync_records_failures_on_exhaustion() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();
        assert_eq!(CIRCUIT_DB.failure_count(), 0);

        let config = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            use_circuit_breaker: true,
            subsystem: Subsystem::Db,
        };

        let _err: DbResult<()> = retry_sync(&config, || {
            Err(DbError::Sqlite("database is locked".to_string()))
        });

        // Should have recorded failures (3 attempts = 3 failures recorded).
        assert!(
            CIRCUIT_DB.failure_count() >= 3,
            "expected at least 3 failures recorded, got {}",
            CIRCUIT_DB.failure_count()
        );

        CIRCUIT_DB.reset();
    }

    // -- retry_sync resets CB on success ------------------------------------

    #[test]
    fn retry_sync_resets_cb_on_success() {
        let _lock = HEALTH_TEST_MUTEX.lock().unwrap();
        CIRCUIT_DB.reset();

        // Inject some failures first.
        CIRCUIT_DB.record_failure();
        CIRCUIT_DB.record_failure();
        assert_eq!(CIRCUIT_DB.failure_count(), 2);

        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            use_circuit_breaker: true,
            subsystem: Subsystem::Db,
        };

        let result = retry_sync(&config, || Ok(42));
        assert_eq!(result.unwrap(), 42);
        // record_success should have cleared failure count.
        assert_eq!(CIRCUIT_DB.failure_count(), 0);

        CIRCUIT_DB.reset();
    }
}
