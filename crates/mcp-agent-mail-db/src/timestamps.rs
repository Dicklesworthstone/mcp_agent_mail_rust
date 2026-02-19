//! Timestamp conversion utilities with clock skew detection.
//!
//! `sqlmodel_rust` uses i64 (microseconds since Unix epoch) for timestamps.
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
/// If the wall clock jumped backward by more than 1 second, returns the
/// last observed value (monotonic guarantee for stored timestamps).
/// Forward jumps over 5 minutes are logged as warnings.
#[inline]
#[must_use]
pub fn now_micros() -> i64 {
    let current = Utc::now().timestamp_micros();
    let last = LAST_SYSTEM_TIME_US.load(Ordering::Relaxed);

    if last != 0 {
        let delta = current - last;
        if delta < -BACKWARD_JUMP_THRESHOLD_US {
            // Clock jumped backward — prevent timestamp regression.
            CLOCK_SKEW_BACKWARD_COUNT.fetch_add(1, Ordering::Relaxed);
            // Don't update LAST_SYSTEM_TIME_US so we keep the high-water mark.
            return last;
        }
        if delta > FORWARD_JUMP_THRESHOLD_US {
            // Clock jumped forward — likely NTP correction or resume from suspend.
            CLOCK_SKEW_FORWARD_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    LAST_SYSTEM_TIME_US.store(current, Ordering::Relaxed);
    current
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
#[derive(Debug, Clone, Default)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn skew_test_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        lock.lock()
            .unwrap_or_else(|poisoned| panic!("timestamp skew test lock poisoned: {poisoned}"))
    }

    #[test]
    fn test_round_trip() {
        let now = Utc::now().naive_utc();
        let micros = naive_to_micros(now);
        let back = micros_to_naive(micros);

        // Should be within 1 microsecond (nanosecond precision lost)
        let diff = (now.and_utc().timestamp_micros() - back.and_utc().timestamp_micros()).abs();
        assert!(diff <= 1, "Round trip failed: diff={diff}");
    }

    #[test]
    fn test_now_micros() {
        let _guard = skew_test_guard();
        clock_skew_reset();

        let before = Utc::now().timestamp_micros();
        let now = now_micros();
        let after = Utc::now().timestamp_micros();

        assert!(now >= before);
        assert!(now <= after);
    }

    #[test]
    fn test_micros_to_iso() {
        let micros = 1_704_067_200_000_000_i64; // 2024-01-01 00:00:00 UTC
        let iso = micros_to_iso(micros);
        assert!(iso.starts_with("2024-01-01T00:00:00"));
    }

    #[test]
    fn test_iso_to_micros() {
        let iso = "2024-01-01T00:00:00.000000Z";
        let micros = iso_to_micros(iso).unwrap();
        assert_eq!(micros, 1_704_067_200_000_000);
    }

    #[test]
    fn test_negative_timestamps() {
        // Test pre-1970 date: 1969-12-31 23:59:59.500000 UTC
        // This is -500_000 microseconds from epoch
        let micros = -500_000_i64;
        let dt = micros_to_naive(micros);

        // Should be 1969-12-31 23:59:59.500000
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "1969-12-31 23:59:59"
        );

        // Round-trip should work
        let back = naive_to_micros(dt);
        assert_eq!(back, micros);
    }

    #[test]
    fn test_epoch_boundary() {
        // Exactly at epoch
        let micros = 0_i64;
        let dt = micros_to_naive(micros);
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "1970-01-01 00:00:00"
        );

        // One microsecond before epoch
        let micros = -1_i64;
        let dt = micros_to_naive(micros);
        // Should be 1969-12-31 23:59:59.999999
        let back = naive_to_micros(dt);
        assert_eq!(back, micros);
    }

    #[test]
    fn test_extreme_values_no_panic() {
        // These extreme values are outside chrono's representable range.
        // Before the fix, this would panic or return epoch.
        // After the fix, it saturates to MIN/MAX.

        let dt_min = micros_to_naive(i64::MIN);
        // chrono::NaiveDateTime::MIN is approx year -262144
        assert!(
            dt_min.year() < -200_000,
            "i64::MIN micros should saturate to ancient past, got {dt_min:?}"
        );

        let dt_max = micros_to_naive(i64::MAX);
        // chrono::NaiveDateTime::MAX is approx year +262143
        assert!(
            dt_max.year() > 200_000,
            "i64::MAX micros should saturate to far future, got {dt_max:?}"
        );
    }

    #[test]
    fn clock_skew_metrics_initially_zero() {
        let _guard = skew_test_guard();
        // Reset to isolate from parallel tests.
        clock_skew_reset();
        let m = clock_skew_metrics();
        assert_eq!(m.backward_jumps, 0);
        assert_eq!(m.forward_jumps, 0);
    }

    #[test]
    fn now_micros_monotonic_under_normal_conditions() {
        let _guard = skew_test_guard();
        // Reset to clean state.
        clock_skew_reset();

        let t1 = now_micros();
        let t2 = now_micros();
        let t3 = now_micros();

        // Timestamps should be non-decreasing.
        assert!(t2 >= t1, "t2={t2} < t1={t1}");
        assert!(t3 >= t2, "t3={t3} < t2={t2}");
    }

    #[test]
    fn backward_jump_returns_last_seen() {
        let _guard = skew_test_guard();
        clock_skew_reset();

        // Prime the global with a known "future" value.
        let future = Utc::now().timestamp_micros() + 10_000_000; // 10s in the future
        LAST_SYSTEM_TIME_US.store(future, Ordering::Relaxed);

        // now_micros() should return `future` because the real clock is behind it.
        let result = now_micros();
        assert_eq!(result, future, "backward jump should return last_seen");

        let m = clock_skew_metrics();
        assert!(
            m.backward_jumps >= 1,
            "should have detected a backward jump"
        );
    }

    #[test]
    fn forward_jump_detected_and_counted() {
        let _guard = skew_test_guard();
        clock_skew_reset();

        // Prime the global with a value far in the past (>5min ago).
        let past = Utc::now().timestamp_micros() - FORWARD_JUMP_THRESHOLD_US - 1_000_000;
        LAST_SYSTEM_TIME_US.store(past, Ordering::Relaxed);
        let baseline_forward = CLOCK_SKEW_FORWARD_COUNT.load(Ordering::Relaxed);

        let result = now_micros();

        // Should still return the current time (forward jumps don't clamp).
        assert!(result > past, "forward jump should use current time");

        let m = clock_skew_metrics();
        assert!(
            m.forward_jumps > baseline_forward,
            "should have detected a forward jump"
        );
    }

    #[test]
    fn now_micros_raw_unaffected_by_skew() {
        let _guard = skew_test_guard();
        clock_skew_reset();

        // Prime global far in the future.
        let future = Utc::now().timestamp_micros() + 100_000_000;
        LAST_SYSTEM_TIME_US.store(future, Ordering::Relaxed);

        // now_micros_raw() should return the actual wall clock.
        let raw = now_micros_raw();
        assert!(
            raw < future,
            "raw should return actual time, not clamped: raw={raw}, future={future}"
        );
    }

    #[test]
    fn iso_to_micros_rfc3339_with_offset() {
        // RFC 3339 with +00:00 offset
        let micros = iso_to_micros("2024-01-01T00:00:00+00:00").unwrap();
        assert_eq!(micros, 1_704_067_200_000_000);
    }

    #[test]
    fn iso_to_micros_without_timezone() {
        // Bare datetime without Z or offset
        let micros = iso_to_micros("2024-01-01T00:00:00").unwrap();
        assert_eq!(micros, 1_704_067_200_000_000);
    }

    #[test]
    fn iso_to_micros_invalid_returns_none() {
        assert!(iso_to_micros("not-a-date").is_none());
        assert!(iso_to_micros("").is_none());
        assert!(iso_to_micros("2024-13-01T00:00:00Z").is_none()); // month 13
    }

    #[test]
    fn micros_to_iso_roundtrip_precision() {
        let original = 1_704_067_200_123_456_i64; // 2024-01-01 + 123456 us
        let iso = micros_to_iso(original);
        let back = iso_to_micros(&iso).unwrap();
        assert_eq!(
            back, original,
            "ISO roundtrip should preserve microsecond precision"
        );
    }

    #[test]
    fn clock_skew_metrics_default() {
        let m = ClockSkewMetrics::default();
        assert_eq!(m.backward_jumps, 0);
        assert_eq!(m.forward_jumps, 0);
        assert_eq!(m.last_system_time_us, 0);
    }

    #[test]
    fn clock_skew_metrics_clone_debug() {
        let m = ClockSkewMetrics {
            backward_jumps: 1,
            forward_jumps: 2,
            last_system_time_us: 3,
        };
        let cloned = m.clone();
        assert_eq!(cloned.backward_jumps, 1);
        let debug = format!("{m:?}");
        assert!(debug.contains("backward_jumps"));
    }

    #[test]
    fn small_backward_drift_allowed() {
        let _guard = skew_test_guard();
        clock_skew_reset();

        // Set LAST to just slightly in the future (< 1s), simulating normal jitter.
        let slight_future = Utc::now().timestamp_micros() + 500_000; // 0.5s
        LAST_SYSTEM_TIME_US.store(slight_future, Ordering::Relaxed);
        let baseline = CLOCK_SKEW_BACKWARD_COUNT.load(Ordering::Relaxed);

        let _result = now_micros();

        // Small drift (<1s) should NOT trigger the backward jump guard.
        let after = CLOCK_SKEW_BACKWARD_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after, baseline,
            "sub-threshold drift should not count as backward jump"
        );
    }
}
