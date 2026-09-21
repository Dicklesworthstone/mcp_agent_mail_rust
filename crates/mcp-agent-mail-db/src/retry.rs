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
//!
//! Retry attempts carry their admission epoch. An operation admitted before an
//! open/reset transition cannot heal or damage its successor epoch. Half-open
//! retry probes are single-flight even when an operation outlives the interval;
//! dropping a probe releases its slot without inventing a successful outcome.

use crate::error::{DbError, DbResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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

/// One coherent observation of a subsystem, not four independent atomic reads.
#[derive(Debug, Clone, Copy)]
pub struct CircuitSnapshot {
    pub state: CircuitState,
    pub failures: u32,
    pub half_open_successes: u32,
    pub remaining_open_secs: f64,
    pub probe_in_flight: bool,
}

#[derive(Debug, Default)]
struct CircuitData {
    failures: u32,
    half_open_successes: u32,
    // None is closed. Some(0) is a valid immediately half-open deadline.
    open_until_us: Option<u64>,
    last_probe_us: Option<u64>,
    // Identity rather than a wrapping generation number. An outstanding attempt
    // retains its Arc, so its identity cannot be reused by a newer generation.
    generation: Arc<()>,
    active_probe: Option<Arc<()>>,
}

impl CircuitData {
    fn state(&self, now_us: u64) -> CircuitState {
        match self.open_until_us {
            None => CircuitState::Closed,
            Some(until) if now_us < until => CircuitState::Open,
            Some(_) => CircuitState::HalfOpen,
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn record_success(&mut self, now_us: u64) {
        match self.state(now_us) {
            CircuitState::Closed => self.failures = 0,
            CircuitState::HalfOpen => {
                self.half_open_successes = self.half_open_successes.saturating_add(1);
                if self.half_open_successes >= HALF_OPEN_SUCCESS_THRESHOLD {
                    self.clear();
                }
            }
            // Concurrent calls admitted while closed can finish after another
            // call opens the circuit. Their success is not a recovery probe.
            CircuitState::Open => {}
        }
    }

    fn record_failure(&mut self, now_us: u64, threshold: u32, reset_duration: Duration) {
        self.failures = self.failures.saturating_add(1);
        self.half_open_successes = 0;
        if self.failures >= threshold {
            let until = now_us.saturating_add(micros_from_duration(reset_duration));
            self.open_until_us = Some(self.open_until_us.map_or(until, |old| old.max(until)));
            self.generation = Arc::new(());
            self.active_probe = None;
            // Retain the last probe timestamp across a failed recovery. A short
            // reset duration must not bypass the minimum probe-start interval.
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CircuitRefusal {
    failures: u32,
    remaining_us: u64,
    half_open: bool,
    probe_in_flight: bool,
}

impl CircuitRefusal {
    fn into_error(self, subsystem: &str) -> DbError {
        #[allow(clippy::cast_precision_loss)]
        let remaining = self.remaining_us as f64 / 1_000_000.0;
        let message = if self.probe_in_flight {
            format!(
                "[{subsystem}] Circuit breaker half-open, recovery probe already in flight. \
                 Retry after that probe completes; its completion time is unknown."
            )
        } else if self.half_open {
            format!(
                "[{subsystem}] Circuit breaker half-open, probe rate-limited. \
                 Next probe in {remaining:.1}s."
            )
        } else {
            format!(
                "[{subsystem}] Circuit breaker open after {} consecutive failures. \
                 Resets in {remaining:.1}s.",
                self.failures,
            )
        };
        DbError::CircuitBreakerOpen {
            message,
            failures: self.failures,
            // For an in-flight probe this is a retry hint, not a completion SLA.
            reset_after_secs: remaining,
        }
    }
}

/// Thread-safe circuit breaker with per-subsystem isolation.
///
/// Related state transitions and admission are serialized in a short critical
/// section. No operation, sleep, or tracing subscriber runs under that lock.
/// Use [`Self::begin_attempt`] for concurrent operations: its single-use result
/// token ties feedback to the admission epoch and owns a half-open probe slot.
#[derive(Debug)]
pub struct CircuitBreaker {
    data: Mutex<CircuitData>,
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
            data: Mutex::new(CircuitData::default()),
            threshold: threshold.max(1),
            reset_duration,
            epoch: Instant::now(),
            subsystem,
        }
    }

    fn lock_data(&self) -> MutexGuard<'_, CircuitData> {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Current state and counters from the same observation.
    #[must_use]
    pub fn snapshot(&self) -> CircuitSnapshot {
        let data = self.lock_data();
        let now_us = self.now_us();
        #[allow(clippy::cast_precision_loss)]
        let remaining_open_secs = data
            .open_until_us
            .map_or(0, |until| until.saturating_sub(now_us)) as f64
            / 1_000_000.0;
        CircuitSnapshot {
            state: data.state(now_us),
            failures: data.failures,
            half_open_successes: data.half_open_successes,
            remaining_open_secs,
            probe_in_flight: data.active_probe.is_some(),
        }
    }

    /// Current circuit state.
    #[must_use]
    pub fn state(&self) -> CircuitState {
        self.snapshot().state
    }

    /// Number of consecutive failures.
    #[must_use]
    pub fn failure_count(&self) -> u32 {
        self.snapshot().failures
    }

    /// Number of consecutive half-open successes (toward the close threshold).
    #[must_use]
    pub fn half_open_success_count(&self) -> u32 {
        self.snapshot().half_open_successes
    }

    /// Seconds remaining until `Open` becomes `HalfOpen`, or zero when not open.
    #[must_use]
    pub fn remaining_open_secs(&self) -> f64 {
        self.snapshot().remaining_open_secs
    }

    fn admit_state(
        data: &mut CircuitData,
        now_us: u64,
    ) -> Result<CircuitState, CircuitRefusal> {
        let state = data.state(now_us);
        let interval_us = micros_from_duration(HALF_OPEN_PROBE_INTERVAL);
        match state {
            CircuitState::Closed => Ok(state),
            CircuitState::Open => Err(CircuitRefusal {
                failures: data.failures,
                remaining_us: data.open_until_us.unwrap_or(now_us).saturating_sub(now_us),
                half_open: false,
                probe_in_flight: false,
            }),
            CircuitState::HalfOpen => {
                if data.active_probe.is_some() {
                    return Err(CircuitRefusal {
                        failures: data.failures,
                        remaining_us: interval_us,
                        half_open: true,
                        probe_in_flight: true,
                    });
                }
                if let Some(last) = data.last_probe_us {
                    let elapsed = now_us.saturating_sub(last);
                    if elapsed < interval_us {
                        return Err(CircuitRefusal {
                            failures: data.failures,
                            remaining_us: interval_us - elapsed,
                            half_open: true,
                            probe_in_flight: false,
                        });
                    }
                }
                data.last_probe_us = Some(now_us);
                Ok(state)
            }
        }
    }

    /// Check admission without retaining an operation token.
    ///
    /// This supports externally serialized observation callers. Concurrent or
    /// cancelable operations must use [`Self::begin_attempt`] instead: a bare
    /// check cannot associate a later result with the epoch that admitted it.
    pub fn check(&self) -> DbResult<()> {
        let admission = {
            let mut data = self.lock_data();
            Self::admit_state(&mut data, self.now_us())
        };
        admission
            .map(|_| ())
            .map_err(|refusal| refusal.into_error(self.subsystem))
    }

    /// Admit one operation and retain its epoch and optional recovery-probe slot.
    /// No lock remains held while the operation executes. A dropped token is
    /// neutral evidence, never a success or a subsystem failure.
    pub fn begin_attempt(&self) -> DbResult<CircuitAttempt<'_>> {
        self.begin_attempt_at(self.now_us())
    }

    fn begin_attempt_at(&self, now_us: u64) -> DbResult<CircuitAttempt<'_>> {
        let admission = {
            let mut data = self.lock_data();
            Self::admit_state(&mut data, now_us).map(|admitted_state| {
                let probe = (admitted_state == CircuitState::HalfOpen).then(|| Arc::new(()));
                data.active_probe.clone_from(&probe);
                CircuitAttempt {
                    breaker: self,
                    generation: Arc::clone(&data.generation),
                    probe,
                    admitted_state,
                    completed: false,
                }
            })
        };
        admission.map_err(|refusal| refusal.into_error(self.subsystem))
    }

    /// Record an unscoped observation from an externally serialized caller.
    /// Open circuits never close on such a success, and this cannot complete an
    /// owned probe. Concurrent retry paths use [`CircuitAttempt::record_success`].
    pub fn record_success(&self) {
        let transition = {
            let mut data = self.lock_data();
            let now_us = self.now_us();
            let before = data.state(now_us);
            if data.active_probe.is_none() {
                data.record_success(now_us);
            }
            (before, data.state(now_us))
        };
        self.log_transition(transition);
    }

    /// Record an unscoped failure observation — may open the circuit.
    pub fn record_failure(&self) {
        self.record_failure_at(self.now_us());
    }

    fn record_failure_at(&self, now_us: u64) {
        let transition = {
            let mut data = self.lock_data();
            let before = data.state(now_us);
            data.record_failure(now_us, self.threshold, self.reset_duration);
            (before, data.state(now_us))
        };
        self.log_transition(transition);
    }

    fn finish_attempt(&self, attempt: &CircuitAttempt<'_>, result: AttemptResult, now_us: u64) {
        let transition = {
            let mut data = self.lock_data();
            if !Arc::ptr_eq(&attempt.generation, &data.generation) {
                return;
            }
            if let Some(probe) = attempt.probe.as_ref() {
                if !data
                    .active_probe
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, probe))
                {
                    return;
                }
                data.active_probe = None;
            }
            let before = data.state(now_us);
            match result {
                AttemptResult::Success => data.record_success(now_us),
                AttemptResult::Failure => {
                    data.record_failure(now_us, self.threshold, self.reset_duration);
                }
                AttemptResult::Abandoned => {
                    // A cancellation/panic/non-retryable result cannot extend
                    // the recovery-success streak. Keep the rate-limit timestamp.
                    if attempt.probe.is_some() {
                        data.half_open_successes = 0;
                    }
                }
            }
            (before, data.state(now_us))
        };
        self.log_transition(transition);
    }

    fn log_transition(&self, (from, to): (CircuitState, CircuitState)) {
        if from != to {
            log_transition(self.subsystem, from, to);
        }
    }

    /// Reset to `Closed` and invalidate all outstanding result tokens.
    pub fn reset(&self) {
        self.lock_data().clear();
    }

    fn now_us(&self) -> u64 {
        micros_from_duration(self.epoch.elapsed())
    }
}

#[derive(Debug, Clone, Copy)]
enum AttemptResult {
    Success,
    Failure,
    Abandoned,
}

/// Single-use feedback for one admitted operation. It can move with an async
/// task or to another thread; it holds no mutex guard. Dropping a half-open
/// attempt releases only its own probe slot, even across resets and retrips.
#[derive(Debug)]
#[must_use = "retain the attempt until the operation completes"]
pub struct CircuitAttempt<'a> {
    breaker: &'a CircuitBreaker,
    generation: Arc<()>,
    probe: Option<Arc<()>>,
    admitted_state: CircuitState,
    completed: bool,
}

impl CircuitAttempt<'_> {
    /// Whether this operation was admitted as a recovery probe.
    #[must_use]
    pub fn is_half_open_probe(&self) -> bool {
        self.admitted_state == CircuitState::HalfOpen
    }

    /// Report successful completion. A stale token does not change its successor.
    pub fn record_success(self) {
        let now_us = self.breaker.now_us();
        self.finish(AttemptResult::Success, now_us);
    }

    /// Report a retryable operation failure, once for the logical operation.
    pub fn record_failure(self) {
        let now_us = self.breaker.now_us();
        self.finish(AttemptResult::Failure, now_us);
    }

    fn finish(mut self, result: AttemptResult, now_us: u64) {
        // Feedback and its tracing callback may unwind. Do not let Drop then
        // reinterpret an already-published result as an abandoned operation.
        self.completed = true;
        self.breaker.finish_attempt(&self, result, now_us);
    }
}

impl Drop for CircuitAttempt<'_> {
    fn drop(&mut self) {
        if !self.completed && self.probe.is_some() {
            self.breaker
                .finish_attempt(self, AttemptResult::Abandoned, self.breaker.now_us());
        }
    }
}

/// Log a circuit state transition at WARN level, outside the circuit lock.
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

/// Alternate public name for the database circuit, not a fifth independent gate.
/// Both names share admission, failure history, configuration and manual reset.
pub use self::CIRCUIT_DB as CIRCUIT_BREAKER;

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
    /// Logical operation name reported in budget-exhaustion envelopes
    /// (default: `"db_operation"`).
    pub operation: &'static str,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
            use_circuit_breaker: true,
            subsystem: Subsystem::Db,
            operation: "db_operation",
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
    let old = SEED.try_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
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
/// `CircuitBreakerOpen` error if the circuit is open. When a retryable error
/// exhausts a non-zero retry budget, the final error is wrapped in
/// [`DbError::RetryBudgetExhausted`] so consumers can report the attempts
/// made and the wall-clock time spent instead of advising another blind
/// retry (br-bvq1x.4.3 / D3).
pub fn retry_sync<T, F>(config: &RetryConfig, op: F) -> DbResult<T>
where
    F: FnMut() -> DbResult<T>,
{
    let cb = if config.use_circuit_breaker {
        Some(circuit_for(config.subsystem))
    } else {
        None
    };
    retry_sync_with_breaker(config, cb, op)
}

fn retry_sync_with_breaker<T, F>(
    config: &RetryConfig,
    cb: Option<&CircuitBreaker>,
    mut op: F,
) -> DbResult<T>
where
    F: FnMut() -> DbResult<T>,
{
    let started = Instant::now();
    let mut last_err = None;

    for attempt in 0..=config.max_retries {
        // Admission state and feedback identity are captured together. A second
        // state() read after check() could refer to another circuit epoch.
        let admission = cb.map(CircuitBreaker::begin_attempt).transpose()?;

        match op() {
            Ok(val) => {
                if let Some(admission) = admission {
                    admission.record_success();
                }
                // Stale feedback is ignored, but an actual successful operation
                // still succeeds. Never retry a committed side effect to heal a circuit.
                return Ok(val);
            }
            Err(e) => {
                let retryable = e.is_retryable();
                if retryable
                    && admission
                        .as_ref()
                        .is_some_and(CircuitAttempt::is_half_open_probe)
                {
                    if let Some(admission) = admission {
                        admission.record_failure();
                    }
                    // A recovery probe is one attempt, not a local retry storm.
                    return Err(e);
                }

                if !retryable || attempt == config.max_retries {
                    if retryable && let Some(admission) = admission {
                        // Charge one exhausted logical operation, not each retry.
                        admission.record_failure();
                    }
                    // Non-retryable errors drop the token as neutral evidence.
                    if retryable && config.max_retries > 0 {
                        return Err(DbError::RetryBudgetExhausted {
                            operation: config.operation,
                            attempts: attempt.saturating_add(1),
                            budget: config.max_retries.saturating_add(1),
                            elapsed_ms: u64::try_from(started.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                            inner: Box::new(e),
                        });
                    }
                    return Err(e);
                }

                // Release admission before sleeping. The next attempt must
                // re-enter the current gate rather than inherit old authority.
                drop(admission);
                last_err = Some(e);
                std::thread::sleep(config.delay_for_attempt(attempt));
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

/// Return current health with coherent per-circuit observations. Different
/// subsystems are sampled independently; the top-level DB fields reuse its sample.
#[must_use]
pub fn db_health_status() -> DbHealthStatus {
    let db_snapshot = CIRCUIT_DB.snapshot();
    let db_state = db_snapshot.state;

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
            let snapshot = if sub == Subsystem::Db {
                db_snapshot
            } else {
                cb.snapshot()
            };
            let state = snapshot.state;
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
                failures: snapshot.failures,
                threshold: cb.threshold(),
                reset_secs: cb.reset_duration().as_secs(),
                half_open_successes: snapshot.half_open_successes,
                recommendation: rec,
            }
        })
        .collect();

    DbHealthStatus {
        circuit_state: db_state.to_string(),
        circuit_failures: db_snapshot.failures,
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
    fn circuit_breaker_clamps_zero_threshold_to_one() {
        let cb = CircuitBreaker::with_params(0, Duration::from_secs(30));
        assert_eq!(cb.threshold(), 1);
        assert_eq!(cb.state(), CircuitState::Closed);

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
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
        if let DbError::CircuitBreakerOpen {
            message,
            reset_after_secs,
            ..
        } = err
        {
            assert!(
                message.contains("rate-limited"),
                "should mention rate limit: {message}"
            );
            assert!(
                reset_after_secs > 0.0,
                "half-open rate limit should report wait time, got {reset_after_secs}"
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
            ..Default::default()
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
            ..Default::default()
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
        // max_retries=3 means 4 attempts total (0..=3)
        assert_eq!(attempt.get(), 4);

        // D3 (br-bvq1x.4.3): a spent budget is wrapped with retry context so
        // downstream envelopes can report attempts/elapsed honestly.
        let err = result.unwrap_err();
        let DbError::RetryBudgetExhausted {
            operation,
            attempts,
            budget,
            inner,
            ..
        } = err
        else {
            panic!("expected RetryBudgetExhausted, got: {err}");
        };
        assert_eq!(operation, "db_operation");
        assert_eq!(attempts, 4);
        assert_eq!(budget, 4);
        assert!(matches!(*inner, DbError::Sqlite(_)));
    }

    #[test]
    fn retry_sync_exhaustion_uses_configured_operation_name() {
        let config = RetryConfig {
            max_retries: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            use_circuit_breaker: false,
            operation: "quick_check",
            ..Default::default()
        };
        let result: DbResult<()> = retry_sync(&config, || {
            Err(DbError::ResourceBusy("database is busy".to_string()))
        });
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                DbError::RetryBudgetExhausted {
                    operation: "quick_check",
                    ..
                }
            ),
            "operation name propagates: {err}"
        );
        // Legacy code and retryability delegate to the wrapped error.
        assert_eq!(err.error_code(), "RESOURCE_BUSY");
        assert!(err.is_retryable());
    }

    #[test]
    fn retry_sync_zero_budget_does_not_wrap() {
        let config = RetryConfig {
            max_retries: 0,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            use_circuit_breaker: false,
            ..Default::default()
        };
        let result: DbResult<()> = retry_sync(&config, || {
            Err(DbError::ResourceBusy("database is busy".to_string()))
        });
        let err = result.unwrap_err();
        assert!(
            matches!(err, DbError::ResourceBusy(_)),
            "single-attempt configs keep the raw error: {err}"
        );
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

    #[test]
    fn record_success_in_open_state_preserves_the_recovery_gate() {
        let cb = CircuitBreaker::with_params(2, Duration::from_mins(5));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        // A pre-trip operation can complete after check() admitted it. Its
        // success cannot replace the required half-open recovery sequence.
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.failure_count(), 2);
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
            ..Default::default()
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
            ..Default::default()
        };

        let _err: DbResult<()> = retry_sync(&config, || {
            Err(DbError::Sqlite("database is locked".to_string()))
        });

        // Exhausting one logical operation should count as one breaker failure,
        // not one failure per internal retry attempt.
        assert_eq!(CIRCUIT_DB.failure_count(), 1);

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
            ..Default::default()
        };

        let result = retry_sync(&config, || Ok(42));
        assert_eq!(result.unwrap(), 42);
        // record_success should have cleared failure count.
        assert_eq!(CIRCUIT_DB.failure_count(), 0);

        CIRCUIT_DB.reset();
    }

    // -- Epoch-scoped admission and retry feedback --------------------------

    #[test]
    fn closed_epoch_success_cannot_count_as_later_recovery_evidence() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        let old = cb.begin_attempt_at(0).unwrap();
        cb.record_failure_at(0);
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        old.finish(AttemptResult::Success, 1);
        assert_eq!(cb.failure_count(), 1);
        assert_eq!(cb.half_open_success_count(), 0);
        for index in 0..HALF_OPEN_SUCCESS_THRESHOLD {
            let now = u64::from(index) * micros_from_duration(HALF_OPEN_PROBE_INTERVAL);
            cb.begin_attempt_at(now)
                .unwrap()
                .finish(AttemptResult::Success, now);
        }
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn reset_invalidates_both_success_and_failure_from_old_attempts() {
        let cb = CircuitBreaker::with_params(5, Duration::from_secs(30));
        let old_success = cb.begin_attempt().unwrap();
        let old_failure = cb.begin_attempt().unwrap();
        cb.reset();
        cb.record_failure();
        old_success.record_success();
        old_failure.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn probe_slot_remains_exclusive_after_its_start_interval_expires() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure_at(0);
        let probe = cb.begin_attempt_at(0).unwrap();
        assert!(probe.is_half_open_probe());
        let later = 10 * micros_from_duration(HALF_OPEN_PROBE_INTERVAL);
        let error = cb.begin_attempt_at(later).unwrap_err();
        assert!(error.to_string().contains("in flight"));
        assert!(cb.snapshot().probe_in_flight);
        drop(probe);
        assert!(!cb.snapshot().probe_in_flight);
        assert!(cb.begin_attempt_at(later).is_ok());
    }

    #[test]
    fn abandoned_probe_keeps_rate_limit_but_breaks_success_streak() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure_at(0);
        let interval = micros_from_duration(HALF_OPEN_PROBE_INTERVAL);
        for now in [0, interval] {
            cb.begin_attempt_at(now)
                .unwrap()
                .finish(AttemptResult::Success, now);
        }
        assert_eq!(cb.half_open_success_count(), 2);
        let probe = cb.begin_attempt_at(2 * interval).unwrap();
        drop(probe);
        assert_eq!(cb.half_open_success_count(), 0);
        assert!(cb.begin_attempt_at(2 * interval).is_err());
        cb.begin_attempt_at(3 * interval)
            .unwrap()
            .finish(AttemptResult::Success, 3 * interval);
        assert_eq!(cb.half_open_success_count(), 1);
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn old_probe_drop_does_not_release_a_successor_epoch_probe() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure_at(0);
        let old = cb.begin_attempt_at(0).unwrap();
        cb.reset();
        cb.record_failure_at(0);
        let current = cb.begin_attempt_at(0).unwrap();
        drop(old);
        assert!(cb.snapshot().probe_in_flight);
        assert!(cb.begin_attempt_at(100_000_000).is_err());
        current.finish(AttemptResult::Success, 0);
        assert_eq!(cb.half_open_success_count(), 1);
    }

    #[test]
    fn unscoped_success_cannot_complete_an_owned_probe() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure_at(0);
        let probe = cb.begin_attempt_at(0).unwrap();
        for _ in 0..10 {
            cb.record_success();
        }
        assert_eq!(cb.half_open_success_count(), 0);
        assert!(cb.snapshot().probe_in_flight);
        probe.finish(AttemptResult::Success, 0);
        assert_eq!(cb.half_open_success_count(), 1);
    }

    #[test]
    fn counters_and_deadlines_saturate_without_fabricating_a_closed_circuit() {
        let cb = CircuitBreaker::with_params(u32::MAX, Duration::MAX);
        cb.lock_data().failures = u32::MAX - 1;
        cb.record_failure_at(u64::MAX - 2);
        cb.record_failure_at(u64::MAX - 1);
        let data = cb.lock_data();
        assert_eq!(data.failures, u32::MAX);
        assert_eq!(data.open_until_us, Some(u64::MAX));
        assert_eq!(data.state(u64::MAX - 1), CircuitState::Open);
    }

    #[test]
    fn retry_preserves_success_without_healing_a_concurrently_opened_circuit() {
        let cb = CircuitBreaker::with_params(1, Duration::from_secs(30));
        let config = RetryConfig::default();
        let calls = std::cell::Cell::new(0);
        let result = retry_sync_with_breaker(&config, Some(&cb), || {
            calls.set(calls.get() + 1);
            cb.record_failure();
            Ok("committed")
        });
        assert_eq!(result.unwrap(), "committed");
        assert_eq!(calls.get(), 1);
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn retry_probe_failure_does_not_spend_the_local_retry_budget() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure();
        let calls = std::cell::Cell::new(0);
        let result: DbResult<()> = retry_sync_with_breaker(
            &RetryConfig::default(),
            Some(&cb),
            || {
                calls.set(calls.get() + 1);
                Err(DbError::ResourceBusy("still unavailable".into()))
            },
        );
        assert!(matches!(result, Err(DbError::ResourceBusy(_))));
        assert_eq!(calls.get(), 1);
        assert_eq!(cb.failure_count(), 2);
        assert!(!cb.snapshot().probe_in_flight);
    }

    #[test]
    fn retry_non_retryable_probe_error_releases_slot_without_charging_failure() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure();
        let result: DbResult<()> = retry_sync_with_breaker(
            &RetryConfig::default(),
            Some(&cb),
            || Err(DbError::not_found("agent", "absent")),
        );
        assert!(result.is_err());
        assert_eq!(cb.failure_count(), 1);
        assert_eq!(cb.half_open_success_count(), 0);
        assert!(!cb.snapshot().probe_in_flight);
        assert!(cb.begin_attempt().is_err(), "abandonment must retain the interval");
    }

    #[test]
    fn panicking_probe_releases_its_slot_without_fabricating_recovery() {
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure();
        let interrupted = std::panic::catch_unwind(|| {
            let _: DbResult<()> = retry_sync_with_breaker(
                &RetryConfig::default(),
                Some(&cb),
                || panic!("operation interrupted"),
            );
        });
        assert!(interrupted.is_err());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert_eq!(cb.failure_count(), 1);
        assert_eq!(cb.half_open_success_count(), 0);
        assert!(!cb.snapshot().probe_in_flight);
        assert!(cb.begin_attempt_at(100_000_000).is_ok());
    }

    #[test]
    fn retry_rechecks_admission_before_repeating_a_failed_operation() {
        let cb = CircuitBreaker::with_params(1, Duration::from_secs(30));
        let config = RetryConfig {
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..RetryConfig::default()
        };
        let calls = std::cell::Cell::new(0);
        let result: DbResult<()> = retry_sync_with_breaker(&config, Some(&cb), || {
            calls.set(calls.get() + 1);
            cb.record_failure();
            Err(DbError::ResourceBusy("concurrent failure opened the gate".into()))
        });
        assert!(matches!(result, Err(DbError::CircuitBreakerOpen { .. })));
        assert_eq!(calls.get(), 1);
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn legacy_and_subsystem_names_share_one_database_breaker() {
        assert!(std::ptr::eq(&*CIRCUIT_BREAKER, &*CIRCUIT_DB));
        assert!(std::ptr::eq(
            &*CIRCUIT_BREAKER,
            circuit_for(Subsystem::Db),
        ));
        assert_eq!(CIRCUIT_BREAKER.threshold(), CIRCUIT_DB.threshold());
        assert_eq!(CIRCUIT_BREAKER.reset_duration(), CIRCUIT_DB.reset_duration());
    }

    #[test]
    fn legacy_failure_blocks_current_retry_and_agrees_with_health_and_reset() {
        let _lock = HEALTH_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct ResetDatabaseCircuit;
        impl Drop for ResetDatabaseCircuit {
            fn drop(&mut self) {
                CIRCUIT_DB.reset();
            }
        }
        let _reset = ResetDatabaseCircuit;
        CIRCUIT_DB.reset();
        for _ in 0..CIRCUIT_DB.threshold() {
            CIRCUIT_BREAKER.record_failure();
        }
        let calls = std::cell::Cell::new(0);
        let result = retry_sync(&RetryConfig::default(), || {
            calls.set(calls.get() + 1);
            Ok(42)
        });
        assert!(matches!(result, Err(DbError::CircuitBreakerOpen { .. })));
        assert_eq!(calls.get(), 0);
        let health = db_health_status();
        let database = health.circuits.iter().find(|status| status.subsystem == "db").unwrap();
        assert_eq!(health.circuit_state, "open");
        assert_eq!(database.state, health.circuit_state);
        assert_eq!(database.failures, CIRCUIT_BREAKER.failure_count());
        assert_eq!(database.failures, health.circuit_failures);
        CIRCUIT_DB.reset();
        assert_eq!(CIRCUIT_BREAKER.state(), CircuitState::Closed);
        assert_eq!(retry_sync(&RetryConfig::default(), || Ok(42)).unwrap(), 42);
        CIRCUIT_BREAKER.record_failure();
        assert_eq!(CIRCUIT_DB.failure_count(), 1);
        CIRCUIT_BREAKER.reset();
        assert_eq!(CIRCUIT_DB.failure_count(), 0);
    }

    #[test]
    fn result_token_moved_to_another_thread_cannot_heal_a_new_epoch() {
        let cb = CircuitBreaker::with_params(1, Duration::from_secs(30));
        let attempt = cb.begin_attempt().unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finish_tx, finish_rx) = std::sync::mpsc::channel();
            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                attempt.record_success();
            });
            let started = started_rx.recv_timeout(Duration::from_secs(5));
            if started.is_ok() {
                cb.record_failure();
            }
            // Unblock and join before asserting so an assertion cannot strand
            // the worker that owns the migrated result token.
            let _ = finish_tx.send(());
            let joined = worker.join();
            assert!(started.is_ok());
            joined.unwrap();
        });
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.failure_count(), 1);
        assert_eq!(cb.half_open_success_count(), 0);
    }

    #[test]
    fn concurrent_failure_observations_do_not_lose_threshold_progress() {
        const WORKERS: u32 = 8;
        const FAILURES_PER_WORKER: u32 = 1_000;
        let threshold = WORKERS * FAILURES_PER_WORKER + 1;
        let cb = CircuitBreaker::with_params(threshold, Duration::from_secs(30));
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    scope.spawn(|| {
                        for _ in 0..FAILURES_PER_WORKER {
                            cb.record_failure();
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert_eq!(cb.failure_count(), threshold - 1);
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        let snapshot = cb.snapshot();
        assert_eq!(snapshot.failures, threshold);
        assert_eq!(snapshot.state, CircuitState::Open);
        assert_eq!(snapshot.half_open_successes, 0);
        assert!(!snapshot.probe_in_flight);
    }

    #[test]
    fn simultaneous_recovery_callers_admit_one_owned_probe() {
        const WORKERS: usize = 32;
        let cb = CircuitBreaker::with_params(1, Duration::ZERO);
        cb.record_failure_at(0);
        std::thread::scope(|scope| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let mut workers = Vec::new();
            for _ in 0..WORKERS {
                let result_tx = result_tx.clone();
                let release = Arc::clone(&release);
                let cb = &cb;
                workers.push(scope.spawn(move || {
                    let attempt = cb.begin_attempt_at(0);
                    let admitted = attempt.is_ok();
                    result_tx.send(admitted).unwrap();
                    if admitted {
                        let (lock, wake) = &*release;
                        let (released, _) = wake
                            .wait_timeout_while(
                                lock.lock().unwrap(),
                                Duration::from_secs(5),
                                |released| !*released,
                            )
                            .unwrap();
                        assert!(*released, "probe owner was not released by the fixture");
                        drop(released);
                    }
                    drop(attempt);
                }));
            }
            drop(result_tx);
            let deadline = Instant::now() + Duration::from_secs(5);
            let observations: Vec<_> = (0..WORKERS)
                .map(|_| result_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())))
                .collect();
            // Release the winner before any fixture assertion, then join all
            // owned workers, including those whose admission was refused.
            let (lock, wake) = &*release;
            *lock.lock().unwrap() = true;
            wake.notify_all();
            let joined: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
            assert!(observations.iter().all(Result::is_ok));
            assert_eq!(observations.iter().filter(|result| matches!(result, Ok(true))).count(), 1);
            assert!(joined.into_iter().all(|result| result.is_ok()));
        });
        assert!(!cb.snapshot().probe_in_flight);
        assert_eq!(cb.half_open_success_count(), 0);
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn runtime_commit_survives_reopen_without_repetition_or_stale_breaker_healing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("committed-operation.sqlite3");
        let path = path.to_str().unwrap();
        let conn = crate::DbConn::open_file(path).expect("open runtime database");
        conn.execute_sync(
            "CREATE TABLE committed_operations (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)",
            &[],
        )
        .expect("create actual runtime table");
        let cb = CircuitBreaker::with_params(1, Duration::from_secs(30));
        let calls = std::cell::Cell::new(0);
        let result = retry_sync_with_breaker(&RetryConfig::default(), Some(&cb), || {
            calls.set(calls.get() + 1);
            conn.execute_sync(
                "INSERT INTO committed_operations (id, payload) VALUES (7, 'durable')",
                &[],
            )
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
            // Another operation's failure arrives after the actual commit but
            // before this successful result reaches the retry wrapper.
            cb.record_failure();
            Ok(7_i64)
        });
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.get(), 1, "a committed operation must not be repeated");
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.failure_count(), 1);
        conn.close_sync().expect("close committed runtime connection");

        let reopened = crate::DbConn::open_file(path).expect("reopen runtime database");
        let rows = reopened
            .query_sync("SELECT id, payload FROM committed_operations ORDER BY id", &[])
            .expect("read actual committed state after reopen");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 7);
        assert_eq!(rows[0].get_named::<String>("payload").unwrap(), "durable");
        reopened.close_sync().expect("close reopened runtime connection");
    }
}
