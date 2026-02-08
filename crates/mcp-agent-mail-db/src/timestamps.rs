//! Timestamp conversion utilities
//!
//! `sqlmodel_rust` uses i64 (microseconds since Unix epoch) for timestamps.
//! This module provides conversion to/from chrono types.

#![allow(clippy::missing_const_for_fn)]

use chrono::{NaiveDateTime, TimeZone, Utc};

/// Microseconds per second
const MICROS_PER_SECOND: i64 = 1_000_000;

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
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
        .naive_utc()
}

/// Get current time as microseconds since Unix epoch.
#[inline]
#[must_use]
pub fn now_micros() -> i64 {
    Utc::now().timestamp_micros()
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
        // Before the fix, this would panic with "valid timestamp".
        // After the fix, it returns Unix epoch as a safe fallback.
        let epoch =
            NaiveDateTime::parse_from_str("1970-01-01 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();

        let dt = micros_to_naive(i64::MAX);
        assert_eq!(dt, epoch);

        let dt = micros_to_naive(i64::MIN);
        assert_eq!(dt, epoch);
    }
}
