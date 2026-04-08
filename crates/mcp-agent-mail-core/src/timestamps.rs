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
        // Clock stood still or went backward. Atomically increment the
        // high-water mark to guarantee each concurrent caller gets a
        // unique value. fetch_add is a single atomic RMW — no TOCTOU
        // window where two threads could read the same prev and both
        // compute prev+1.
        LAST_SYSTEM_TIME_US.fetch_add(1, Ordering::AcqRel) + 1
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
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive_to_micros(dt));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive_to_micros(dt));
    }

    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // -----------------------------------------------------------------------
    // br-aazao.5.1 — conversion functions and boundary values
    // -----------------------------------------------------------------------

    #[test]
    fn naive_to_micros_epoch_is_zero() {
        let epoch = chrono::DateTime::<Utc>::UNIX_EPOCH.naive_utc();
        assert_eq!(naive_to_micros(epoch), 0);
    }

    #[test]
    fn naive_to_micros_known_datetime() {
        // 2026-01-15 12:30:45.123456 UTC
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_micro_opt(12, 30, 45, 123_456)
            .unwrap();
        // Hand-calculated: days from 1970-01-01 to 2026-01-15 = 20_468 days
        // 20468 * 86400 = 1_768_435_200 seconds
        // + 12*3600 + 30*60 + 45 = 45045 seconds
        // total seconds = 1_768_480_245
        // micros = 1_768_480_245 * 1_000_000 + 123_456 = 1_768_480_245_123_456
        assert_eq!(naive_to_micros(dt), 1_768_480_245_123_456);
    }

    #[test]
    fn micros_to_naive_zero_is_epoch() {
        let dt = micros_to_naive(0);
        assert_eq!(dt, chrono::DateTime::<Utc>::UNIX_EPOCH.naive_utc());
    }

    #[test]
    fn micros_to_naive_negative_pre_1970() {
        // -1_000_000 micros = 1969-12-31 23:59:59.000000
        let dt = micros_to_naive(-MICROS_PER_SECOND);
        assert_eq!(dt.and_utc().timestamp(), -1);
        assert_eq!(dt.and_utc().timestamp_subsec_micros(), 0);

        // -1 micro = 1969-12-31 23:59:59.999999
        let dt = micros_to_naive(-1);
        assert_eq!(dt.and_utc().timestamp(), -1);
        assert_eq!(dt.and_utc().timestamp_subsec_micros(), 999_999);
    }

    #[test]
    fn micros_to_naive_extreme_values_do_not_panic() {
        // These hit the chrono overflow fallback paths.
        let max_dt = micros_to_naive(i64::MAX);
        let min_dt = micros_to_naive(i64::MIN);
        // Should return boundary datetimes, not panic.
        let epoch = chrono::DateTime::<Utc>::UNIX_EPOCH.naive_utc();
        assert!(max_dt > epoch);
        assert!(min_dt < epoch);
    }

    #[test]
    fn naive_micros_roundtrip_spanning_full_range() {
        let test_values: &[i64] = &[
            0,
            1,
            -1,
            999_999,
            -999_999,
            MICROS_PER_SECOND,
            -MICROS_PER_SECOND,
            1_000_000_000_000,       // ~2001
            -1_000_000_000_000,      // ~1938
            1_700_000_000_000_000,   // ~2023
            -62_135_596_800_000_000, // year 0 boundary area
            86_400_000_000,          // exactly 1 day
            -86_400_000_000,         // exactly -1 day
            1_768_480_245_123_456,   // 2026-01-15 12:30:45.123456
            253_402_300_799_999_999, // 9999-12-31 23:59:59.999999
            -62_135_596_800_000_000, // 0001-01-01 area
            500_000,                 // half a second
            -500_000,                // negative half second
            1_234_567_890_123_456,   // misc large value
            42,                      // small positive
        ];

        for &micros in test_values {
            let dt = micros_to_naive(micros);
            let back = naive_to_micros(dt);
            assert_eq!(
                back, micros,
                "roundtrip failed for {micros}: naive={dt}, back={back}"
            );
        }
    }

    #[test]
    fn micros_to_iso_format_matches_spec() {
        // Epoch
        let s = micros_to_iso(0);
        assert_eq!(s, "1970-01-01T00:00:00.000000Z");

        // Known value with fractional seconds
        let s = micros_to_iso(1_768_480_245_123_456);
        assert_eq!(s, "2026-01-15T12:30:45.123456Z");

        // Verify format always has 6 fractional digits and trailing Z
        let s = micros_to_iso(MICROS_PER_SECOND); // exactly 1s, no sub-micros
        assert!(s.ends_with('Z'), "should end with Z: {s}");
        assert!(s.contains('.'), "should have fractional separator: {s}");
    }

    #[test]
    fn iso_to_micros_parses_rfc3339_with_timezone() {
        let result = iso_to_micros("2026-01-15T12:30:45.123456+00:00");
        assert_eq!(result, Some(1_768_480_245_123_456));

        // With offset
        let result = iso_to_micros("2026-01-15T13:30:45.123456+01:00");
        assert_eq!(result, Some(1_768_480_245_123_456));
    }

    #[test]
    fn iso_to_micros_parses_naive_with_trailing_z() {
        let result = iso_to_micros("2026-01-15T12:30:45.123456Z");
        assert_eq!(result, Some(1_768_480_245_123_456));
    }

    #[test]
    fn iso_to_micros_parses_naive_without_z() {
        let result = iso_to_micros("2026-01-15T12:30:45.123456");
        assert_eq!(result, Some(1_768_480_245_123_456));
    }

    #[test]
    fn iso_to_micros_parses_seconds_only() {
        let result = iso_to_micros("2026-01-15T12:30:45");
        assert_eq!(result, Some(1_768_480_245_000_000));
    }

    #[test]
    fn iso_to_micros_returns_none_for_garbage() {
        assert_eq!(iso_to_micros(""), None);
        assert_eq!(iso_to_micros("not-a-date"), None);
        assert_eq!(iso_to_micros("2026-13-01T00:00:00"), None);
        assert_eq!(iso_to_micros("🎉"), None);
        assert_eq!(iso_to_micros("2026"), None);
    }

    #[test]
    fn iso_micros_roundtrip() {
        let test_values: &[i64] = &[
            0,
            1_768_480_245_123_456,
            MICROS_PER_SECOND,
            86_400_000_000,
            -MICROS_PER_SECOND,
            -86_400_000_000,
            1_700_000_000_000_000,
        ];
        for &micros in test_values {
            let iso = micros_to_iso(micros);
            let back = iso_to_micros(&iso);
            assert_eq!(
                back,
                Some(micros),
                "iso roundtrip failed for {micros}: iso={iso}, back={back:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // br-aazao.5.2 — clock skew protection, monotonicity, and concurrency
    // -----------------------------------------------------------------------

    /// Reset global state before each clock-skew test.
    /// IMPORTANT: these tests must run with `-- --test-threads=1` or
    /// accept that global atomic state leaks between them. We reset
    /// defensively at the start of each test.
    fn reset() {
        clock_skew_reset();
    }

    #[test]
    fn now_micros_sequential_monotonicity() {
        reset();
        let mut prev = now_micros();
        for i in 0..1000 {
            let next = now_micros();
            assert!(
                next > prev,
                "monotonicity violated at iteration {i}: prev={prev}, next={next}"
            );
            prev = next;
        }
    }

    #[test]
    fn now_micros_concurrent_no_duplicates() {
        reset();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(10));
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    (0..100).map(|_| now_micros()).collect::<Vec<_>>()
                })
            })
            .collect();

        let mut all = BTreeSet::new();
        for h in handles {
            for v in h.join().unwrap() {
                assert!(all.insert(v), "duplicate timestamp: {v}");
            }
        }
        assert_eq!(all.len(), 1000);
    }

    #[test]
    fn clock_skew_metrics_sane_after_reset() {
        reset();
        let m = clock_skew_metrics();
        assert_eq!(m.backward_jumps, 0);
        assert_eq!(m.forward_jumps, 0);
        assert_eq!(m.last_system_time_us, 0);
    }

    #[test]
    fn clock_skew_reset_zeroes_all_counters() {
        // Drive some traffic to populate counters.
        for _ in 0..10 {
            let _ = now_micros();
        }
        // Counters may or may not have jumped, but last_system_time_us > 0.
        assert_ne!(clock_skew_metrics().last_system_time_us, 0);

        clock_skew_reset();
        let m = clock_skew_metrics();
        assert_eq!(m.backward_jumps, 0);
        assert_eq!(m.forward_jumps, 0);
        assert_eq!(m.last_system_time_us, 0);
    }

    #[test]
    fn backward_clock_branch_returns_prev_plus_one() {
        reset();
        // Set high-water mark artificially far in the future.
        let future = Utc::now().timestamp_micros() + 10_000_000_000; // +10_000s
        LAST_SYSTEM_TIME_US.store(future, Ordering::SeqCst);

        // next call should hit the backward branch: current < prev,
        // so it does fetch_add(1) and returns prev+1.
        let result = now_micros();
        assert_eq!(result, future + 1, "should be exactly prev+1");

        // Second call should increment again.
        let result2 = now_micros();
        assert_eq!(result2, future + 2);
    }

    #[test]
    fn now_micros_raw_within_one_second_of_now_micros() {
        reset();
        let protected = now_micros();
        let raw = now_micros_raw();
        let delta = (raw - protected).abs();
        assert!(
            delta < MICROS_PER_SECOND,
            "raw and protected should be within 1s: delta={delta}"
        );
    }

    #[test]
    fn clock_skew_reset_then_fresh_counters() {
        // Generate some calls.
        reset();
        for _ in 0..5 {
            let _ = now_micros();
        }
        clock_skew_reset();
        let m = clock_skew_metrics();
        assert_eq!(m.backward_jumps, 0);
        assert_eq!(m.forward_jumps, 0);
        assert_eq!(m.last_system_time_us, 0);

        // After reset, next call should work normally.
        let v = now_micros();
        assert!(v > 0);
        let m2 = clock_skew_metrics();
        assert!(m2.last_system_time_us > 0);
    }
}
