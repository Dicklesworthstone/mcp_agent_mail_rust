//! Process memory (RSS) sampling and pressure classification.
//!
//! Reads `/proc/self/status` on Linux and the system `ps` RSS field on macOS,
//! without unsafe code. Returns the current resident set, not a lifetime peak,
//! so the background health worker can observe both pressure and recovery.

use crate::Config;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

/// Memory pressure levels, matching `DiskPressure` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    /// RSS below warning threshold — normal operation.
    Ok,
    /// RSS above warning threshold — log warning, consider reducing cache TTL.
    Warning,
    /// RSS above critical threshold — evict cache, increase drain rates.
    Critical,
    /// RSS above fatal threshold — reject new work, shed load.
    Fatal,
}

impl MemoryPressure {
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Critical => 2,
            Self::Fatal => 3,
        }
    }

    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        match v {
            1 => Self::Warning,
            2 => Self::Critical,
            3 => Self::Fatal,
            _ => Self::Ok,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Fatal => "fatal",
        }
    }
}

/// Snapshot of process memory state at a point in time.
#[derive(Debug, Clone)]
pub struct MemorySample {
    /// Resident Set Size in bytes (physical RAM used by this process).
    pub rss_bytes: Option<u64>,
    /// Classified pressure level based on config thresholds.
    pub pressure: MemoryPressure,
    /// Best-effort error if RSS could not be read.
    pub error: Option<String>,
}

/// Classify memory pressure from RSS bytes and config thresholds (in MB).
/// A threshold of 0 means that level is disabled.
#[must_use]
pub const fn classify_pressure(
    rss_bytes: u64,
    warning_mb: u64,
    critical_mb: u64,
    fatal_mb: u64,
) -> MemoryPressure {
    let fatal = fatal_mb.saturating_mul(MIB);
    let critical = critical_mb.saturating_mul(MIB);
    let warning = warning_mb.saturating_mul(MIB);

    if fatal > 0 && rss_bytes > fatal {
        MemoryPressure::Fatal
    } else if critical > 0 && rss_bytes > critical {
        MemoryPressure::Critical
    } else if warning > 0 && rss_bytes > warning {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Ok
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn parse_rss_kib(raw: &str) -> Result<u64, String> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("RSS must contain exactly one unsigned KiB value".to_string());
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(|kib| kib.checked_mul(1024))
        .ok_or_else(|| "RSS value overflows its byte representation".to_string())
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_rss(status: &str) -> Result<u64, String> {
    let rest = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .ok_or_else(|| "VmRSS line not found in /proc/self/status".to_string())?;
    let mut fields = rest.split_ascii_whitespace();
    let value = fields.next().unwrap_or_default();
    if fields.next().is_some_and(|unit| !matches!(unit, "kB" | "KB")) || fields.next().is_some() {
        return Err("unexpected units or trailing fields in VmRSS".to_string());
    }
    parse_rss_kib(value)
}

#[cfg(any(target_os = "macos", test))]
fn parse_ps_rss(stdout: &[u8]) -> Result<u64, String> {
    let value =
        std::str::from_utf8(stdout).map_err(|error| format!("decode ps RSS output: {error}"))?;
    parse_rss_kib(value)
}

#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn run_rss_probe(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<u64, String> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = command
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start ps RSS probe: {error}"))?;
    let started = Instant::now();
    loop {
        let error = match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("ps RSS probe exited with {status}")),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(
                    Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())),
                );
                continue;
            }
            Ok(None) => "ps RSS probe timed out".to_string(),
            Err(error) => format!("wait for ps RSS probe: {error}"),
        };
        // Do not leave a running child or a zombie after a failed sample.
        // Reaping still depends on the OS; this is not a hard real-time bound.
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("read ps RSS output: {error}"))?;
    parse_ps_rss(&output.stdout)
}

#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn read_ps_rss_bytes() -> Result<u64, String> {
    // Absolute system binary, no shell, no PATH lookup, and exactly our PID.
    // Darwin's rss field is current resident memory in 1024-byte units.
    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-o", "rss=", "-p", &std::process::id().to_string()]);
    run_rss_probe(&mut command, std::time::Duration::from_millis(250))
}

/// Read current process RSS on Linux or macOS, in bytes.
///
/// Unsupported platforms, failed probes, malformed output, and overflow return
/// an error rather than a fabricated zero or a lifetime high-water mark.
pub(crate) fn read_rss_bytes() -> Result<u64, String> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status")
            .map_err(|e| format!("read /proc/self/status: {e}"))?;

        parse_linux_rss(&status)
    }

    #[cfg(target_os = "macos")]
    {
        read_ps_rss_bytes()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err("RSS reading not implemented on this platform".to_string())
    }
}

/// Sample current process memory usage and classify pressure.
#[must_use]
pub fn sample_memory(config: &Config) -> MemorySample {
    match read_rss_bytes() {
        Ok(rss_bytes) => {
            let pressure = classify_pressure(
                rss_bytes,
                config.memory_warning_mb,
                config.memory_critical_mb,
                config.memory_fatal_mb,
            );
            MemorySample {
                rss_bytes: Some(rss_bytes),
                pressure,
                error: None,
            }
        }
        Err(e) => MemorySample {
            rss_bytes: None,
            pressure: MemoryPressure::Ok,
            error: Some(e),
        },
    }
}

fn now_unix_micros_u64() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// Sample memory and update global system metrics gauges.
#[must_use]
pub fn sample_and_record(config: &Config) -> MemorySample {
    let sample = sample_memory(config);
    let metrics = crate::global_metrics();

    if let Some(rss) = sample.rss_bytes {
        metrics.system.memory_rss_bytes.set(rss);
    }
    metrics
        .system
        .memory_pressure_level
        .set(sample.pressure.as_u64());
    metrics
        .system
        .memory_last_sample_us
        .set(now_unix_micros_u64());
    if sample.error.is_some() {
        metrics.system.memory_sample_errors_total.add(1);
    }

    sample
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_kib_conversion_is_checked_and_rejects_ambiguous_values() {
        assert_eq!(parse_rss_kib(" 123456 \n").unwrap(), 126_418_944);
        assert_eq!(parse_rss_kib("0").unwrap(), 0);
        let largest = u64::MAX / 1024;
        assert_eq!(parse_rss_kib(&largest.to_string()).unwrap(), largest * 1024);
        assert!(parse_rss_kib(&(largest + 1).to_string()).is_err());
        for invalid in ["", "-1", "+1", "1.5", "1 2", "1\n2", "18446744073709551616"] {
            assert!(parse_rss_kib(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn linux_rss_parser_selects_current_rss_not_peak_or_virtual_size() {
        let status = "Name:\tagent-mail\nVmPeak:\t999999 kB\nVmHWM:\t888888 kB\nVmRSS:\t1234 kB\nVmSize:\t777777 kB\n";
        assert_eq!(parse_linux_rss(status).unwrap(), 1234 * 1024);
        assert_eq!(parse_linux_rss("VmRSS: 12 KB").unwrap(), 12 * 1024);
        assert_eq!(parse_linux_rss("VmRSS: 12").unwrap(), 12 * 1024);
    }

    #[test]
    fn linux_rss_parser_rejects_missing_bad_units_and_overflow() {
        for invalid in [
            "VmSize: 123 kB",
            "VmRSS:",
            "VmRSS: 123 MB",
            "VmRSS: 123 kB extra",
            "VmRSS: -123 kB",
            "VmRSS: 18446744073709551615 kB",
        ] {
            assert!(parse_linux_rss(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn ps_rss_parser_requires_one_headerless_process_row() {
        assert_eq!(parse_ps_rss(b"  4096\n").unwrap(), 4 * MIB);
        for invalid in [b"".as_slice(), b"RSS\n4096\n", b"4096\n8192\n", b"N/A", b"\xff"] {
            assert!(parse_ps_rss(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ps_rss_probe_reads_the_current_process() {
        assert!(read_ps_rss_bytes().expect("system ps should report our RSS") > 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rss_probe_reports_exit_failure_and_stops_timed_out_children() {
        use std::process::Command;
        use std::time::Duration;

        let error = run_rss_probe(&mut Command::new("/usr/bin/false"), Duration::from_secs(1))
            .expect_err("failed probe must not become a zero RSS sample");
        assert!(error.contains("exited"), "{error}");

        let mut sleeper = Command::new("/bin/sleep");
        sleeper.arg("60");
        let error = run_rss_probe(&mut sleeper, Duration::ZERO)
            .expect_err("a stuck probe must be terminated and reaped");
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn pressure_classification_thresholds() {
        // All thresholds in MB, RSS in bytes
        assert_eq!(
            classify_pressure(500 * MIB, 2048, 4096, 8192),
            MemoryPressure::Ok
        );
        assert_eq!(
            classify_pressure(3000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Warning
        );
        assert_eq!(
            classify_pressure(5000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Critical
        );
        assert_eq!(
            classify_pressure(9000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Fatal
        );
    }

    #[test]
    fn pressure_disabled_thresholds() {
        // Threshold of 0 means disabled
        assert_eq!(classify_pressure(10_000 * MIB, 0, 0, 0), MemoryPressure::Ok,);
        // Only warning enabled
        assert_eq!(
            classify_pressure(3000 * MIB, 2048, 0, 0),
            MemoryPressure::Warning,
        );
    }

    #[test]
    fn pressure_label_roundtrip() {
        for (level, expected) in [
            (MemoryPressure::Ok, "ok"),
            (MemoryPressure::Warning, "warning"),
            (MemoryPressure::Critical, "critical"),
            (MemoryPressure::Fatal, "fatal"),
        ] {
            assert_eq!(level.label(), expected);
            assert_eq!(MemoryPressure::from_u64(level.as_u64()), level);
        }
    }

    #[test]
    fn pressure_u64_roundtrip() {
        for v in 0..=3 {
            let p = MemoryPressure::from_u64(v);
            assert_eq!(p.as_u64(), v);
        }
        // Unknown values default to Ok
        assert_eq!(MemoryPressure::from_u64(99), MemoryPressure::Ok);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_rss_returns_nonzero() {
        let rss = read_rss_bytes().expect("should read RSS on Linux and macOS");
        assert!(rss > 0, "RSS should be > 0, got {rss}");
        // Sanity check: a Rust test process should use at least 1MB
        assert!(rss > MIB, "RSS {rss} seems too small");
    }

    #[test]
    fn sample_memory_with_default_config() {
        let config = Config::default();
        let sample = sample_memory(&config);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(sample.rss_bytes.is_some());
            assert!(sample.error.is_none());
        }
    }

    // ── br-3h13: Additional memory.rs test coverage ────────────────

    #[test]
    fn classify_pressure_exactly_at_warning_boundary() {
        // When RSS == warning threshold exactly, it's NOT above so should be Ok
        let threshold = 2048;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, threshold, 4096, 8192),
            MemoryPressure::Ok
        );
        assert_eq!(
            classify_pressure(at_threshold + 1, threshold, 4096, 8192),
            MemoryPressure::Warning
        );
    }

    #[test]
    fn classify_pressure_exactly_at_critical_boundary() {
        let threshold = 4096;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 2048, threshold, 8192),
            MemoryPressure::Warning
        );
        assert_eq!(
            classify_pressure(at_threshold + 1, 2048, threshold, 8192),
            MemoryPressure::Critical
        );
    }

    #[test]
    fn classify_pressure_exactly_at_fatal_boundary() {
        let threshold = 8192;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 2048, 4096, threshold),
            MemoryPressure::Critical
        );
        assert_eq!(
            classify_pressure(at_threshold + 1, 2048, 4096, threshold),
            MemoryPressure::Fatal
        );
    }

    #[test]
    fn classify_pressure_saturating_mul_no_panic() {
        // u64::MAX * MIB saturates to u64::MAX; rss == threshold means NOT above,
        // so all checks fail and result is Ok. Key point: no panic from overflow.
        assert_eq!(
            classify_pressure(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            MemoryPressure::Ok
        );
    }

    #[test]
    fn classify_pressure_zero_rss_always_ok() {
        assert_eq!(classify_pressure(0, 2048, 4096, 8192), MemoryPressure::Ok);
    }

    #[test]
    fn memory_pressure_from_u64_all_unknown_values() {
        for v in [4, 5, 100, 255, u64::MAX] {
            assert_eq!(MemoryPressure::from_u64(v), MemoryPressure::Ok);
        }
    }

    #[test]
    fn memory_sample_error_path() {
        // Construct a sample with error (simulating non-Linux platform)
        let sample = MemorySample {
            rss_bytes: None,
            pressure: MemoryPressure::Ok,
            error: Some("not available".into()),
        };
        assert!(sample.rss_bytes.is_none());
        assert!(sample.error.is_some());
        assert_eq!(sample.pressure, MemoryPressure::Ok);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sample_memory_rss_within_reasonable_range() {
        let config = Config::default();
        let sample = sample_memory(&config);
        let rss = sample.rss_bytes.expect("should have RSS on Linux and macOS");
        // A Rust test process should use between 1 MB and 10 GB
        assert!(rss > MIB, "RSS too small: {rss}");
        assert!(rss < 10 * 1024 * MIB, "RSS too large: {rss}");
    }

    #[test]
    fn sample_memory_with_extreme_thresholds() {
        // warning_mb = 1 means warning threshold = 1 MiB; any real process exceeds that
        let config = Config {
            memory_warning_mb: 1,
            memory_critical_mb: 0,
            memory_fatal_mb: 0,
            ..Config::default()
        };
        let sample = sample_memory(&config);
        if sample.rss_bytes.is_some() {
            assert_eq!(sample.pressure, MemoryPressure::Warning);
        }
    }

    #[test]
    fn sample_and_record_updates_memory_metrics() {
        let config = Config::default();
        let metrics = crate::global_metrics();
        metrics.system.memory_rss_bytes.set(0);
        metrics.system.memory_pressure_level.set(0);
        metrics.system.memory_last_sample_us.set(0);
        metrics.system.memory_sample_errors_total.store(0);

        let sample = sample_and_record(&config);

        assert_eq!(
            metrics.system.memory_pressure_level.load(),
            sample.pressure.as_u64()
        );
        assert!(metrics.system.memory_last_sample_us.load() > 0);

        if let Some(rss) = sample.rss_bytes {
            assert_eq!(metrics.system.memory_rss_bytes.load(), rss);
            assert_eq!(metrics.system.memory_sample_errors_total.load(), 0);
        } else {
            assert_eq!(metrics.system.memory_rss_bytes.load(), 0);
            assert_eq!(metrics.system.memory_sample_errors_total.load(), 1);
            assert!(sample.error.is_some());
        }
    }
}
