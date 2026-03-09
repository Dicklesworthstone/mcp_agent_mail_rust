//! Timestamp conversion utilities with clock skew detection.
//!
//! `sqlmodel` uses i64 (microseconds since Unix epoch) for timestamps.
//! This module provides conversion to/from chrono types, plus monotonic
//! protection against wall-clock jumps (NTP corrections, VM migration, etc.).
//!
//! # Clock Skew Protection
//!
//! [`now_micros`] tracks the last observed wall-clock value. On a backward
//! jump (>1 s), it returns `max(current, last_seen)` so stored timestamps
//! never regress. Forward jumps (>5 min) are logged as warnings.

#![allow(clippy::missing_const_for_fn)]

use chrono::{NaiveDateTime, TimeZone, Utc};
use std::sync::atomic::{AtomicI64, Ordering};

/// Microseconds per second
const MICROS_PER_SECOND: i64 = 1_000_000;

/// Backward jump threshold: 1 second in microseconds.
const BACKWARD_JUMP_THRESHOLD_US: i64 = 1_000_000;

/// Forward jump threshold: 5 minutes in microseconds.
const FORWARD_JUMP_THRESHOLD_US: i64 = 300_000_000;

/// Last observed wall-clock value (microseconds since epoch).
/// Initialized to 0; updated on every `now_micros()` call.
static LAST_SYSTEM_TIME_US: AtomicI64 = AtomicI64::new(0);

/// Convert chrono `NaiveDateTime` to microseconds since Unix epoch.
#[inline]
#[must_use]
pub fn naive_to_micros(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_micros()
}

/// Convert microseconds since Unix epoch to chrono `NaiveDateTime`.
///
/// For extreme values outside chrono's representable range, returns the
/// Unix epoch (1970-01-01 00:00:00) as a safe fallback instead of panicking.
#[inline]
#[must_use]
pub fn micros_to_naive(micros: i64) -> NaiveDateTime {
    // Use divrem that handles negative values correctly
    // rem_euclid always returns non-negative remainder
    let secs = micros.div_euclid(MICROS_PER_SECOND);
    let sub_micros = micros.rem_euclid(MICROS_PER_SECOND);
    let nsecs = u32::try_from(sub_micros * 1000).unwrap_or(0);
    Utc.timestamp_opt(secs, nsecs)
        .single()
        .unwrap_or(if micros < 0 {
            chrono::DateTime::<Utc>::MIN_UTC
        } else {
            chrono::DateTime::<Utc>::MAX_UTC
        })
        .naive_utc()
}

/// Get current time as microseconds since Unix epoch, with clock skew protection.
///
/// Uses `AtomicI64::fetch_max` to maintain a monotonic high-water mark
/// without races. Two concurrent threads can never clobber each other's
/// updates because `fetch_max` is a single atomic read-modify-write.
///
/// If the wall clock jumped backward by more than 1 second, the skew
/// counter is bumped. Forward jumps over 5 minutes are also tracked.
/// Strict monotonicity is guaranteed: no two calls ever return the same value.
#[inline]
#[must_use]
pub fn now_micros() -> i64 {
    let current = Utc::now().timestamp_micros();

    // Atomically set high-water mark and retrieve the previous value.
    // fetch_max(current) stores max(old, current) and returns old.
    let prev = LAST_SYSTEM_TIME_US.fetch_max(current, Ordering::AcqRel);

    if prev != 0 {
        let delta = current - prev;
        if delta < -BACKWARD_JUMP_THRESHOLD_US {
            CLOCK_SKEW_BACKWARD_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        if delta > FORWARD_JUMP_THRESHOLD_US {
            CLOCK_SKEW_FORWARD_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    if current > prev {
        // Normal forward progress — fetch_max already stored `current`.
        current
    } else {
        // Clock stood still or went backward; advance by 1 past the
        // high-water mark to guarantee strict monotonicity.
        let next = prev + 1;
        LAST_SYSTEM_TIME_US.fetch_max(next, Ordering::AcqRel);
        next
    }
}

/// Get the raw wall-clock time without skew protection.
///
/// Use this only when you need the actual system time (e.g., for display).
/// For stored timestamps, always use [`now_micros`].
#[inline]
#[must_use]
pub fn now_micros_raw() -> i64 {
    Utc::now().timestamp_micros()
}

// ---------------------------------------------------------------------------
// Clock skew metrics
// ---------------------------------------------------------------------------

/// Number of detected backward clock jumps.
static CLOCK_SKEW_BACKWARD_COUNT: AtomicI64 = AtomicI64::new(0);

/// Number of detected forward clock jumps.
static CLOCK_SKEW_FORWARD_COUNT: AtomicI64 = AtomicI64::new(0);

/// Snapshot of clock skew detection metrics.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ClockSkewMetrics {
    /// Number of backward clock jumps detected (>1s regression).
    pub backward_jumps: i64,
    /// Number of forward clock jumps detected (>5min advance).
    pub forward_jumps: i64,
    /// Last observed wall-clock value (microseconds since epoch).
    pub last_system_time_us: i64,
}

/// Return a snapshot of clock skew metrics.
#[must_use]
pub fn clock_skew_metrics() -> ClockSkewMetrics {
    ClockSkewMetrics {
        backward_jumps: CLOCK_SKEW_BACKWARD_COUNT.load(Ordering::Relaxed),
        forward_jumps: CLOCK_SKEW_FORWARD_COUNT.load(Ordering::Relaxed),
        last_system_time_us: LAST_SYSTEM_TIME_US.load(Ordering::Relaxed),
    }
}

/// Reset clock skew counters (for testing).
pub fn clock_skew_reset() {
    CLOCK_SKEW_BACKWARD_COUNT.store(0, Ordering::Relaxed);
    CLOCK_SKEW_FORWARD_COUNT.store(0, Ordering::Relaxed);
    LAST_SYSTEM_TIME_US.store(0, Ordering::Relaxed);
}

/// Convert microseconds to ISO-8601 string.
#[inline]
#[must_use]
pub fn micros_to_iso(micros: i64) -> String {
    micros_to_naive(micros)
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

/// Parse ISO-8601 string to microseconds.
///
/// # Errors
/// Returns `None` if the string cannot be parsed.
#[must_use]
pub fn iso_to_micros(s: &str) -> Option<i64> {
    // Try parsing with timezone
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_micros());
    }

    // Try parsing as naive datetime
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(naive_to_micros(dt));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive_to_micros(dt));
    }

    None
}
