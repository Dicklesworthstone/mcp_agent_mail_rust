//! Background worker for disk and memory pressure monitoring.
//!
//! Memory has its own sampling cadence and remains active when disk probes are
//! disabled. Both feed `health_check` and `resource://tooling/metrics_core`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::DiskPressure;
use mcp_agent_mail_core::memory::MemoryPressure;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
const STARTUP_WARN_BYTES: u64 = 1024 * 1024 * 1024; // 1GiB
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Independent elapsed-time schedules; large configured disk intervals must
/// neither delay RSS sampling nor overflow an `Instant` deadline.
struct MonitorSchedule {
    disk_enabled: bool,
    disk_interval: Duration,
    last_disk_sample: Duration,
    last_memory_sample: Duration,
}

impl MonitorSchedule {
    fn new(config: &Config) -> Self {
        Self {
            disk_enabled: config.disk_space_monitor_enabled,
            disk_interval: monitor_interval_seconds(config.disk_space_check_interval_seconds),
            // start() seeds both enabled probes before spawning the worker.
            last_disk_sample: Duration::ZERO,
            last_memory_sample: Duration::ZERO,
        }
    }

    /// Return (disk_due, memory_due), coalescing missed intervals into one probe.
    fn due(&mut self, elapsed: Duration) -> (bool, bool) {
        let disk_due = self.disk_enabled
            && elapsed.saturating_sub(self.last_disk_sample) >= self.disk_interval;
        let memory_due = elapsed.saturating_sub(self.last_memory_sample) >= MEMORY_SAMPLE_INTERVAL;
        if disk_due {
            self.last_disk_sample = elapsed;
        }
        if memory_due {
            self.last_memory_sample = elapsed;
        }
        (disk_due, memory_due)
    }
}

#[inline]
const fn monitor_interval_seconds(seconds: u64) -> Duration {
    Duration::from_secs(if seconds > 5 { seconds } else { 5 })
}

#[inline]
const fn should_emit_startup_warning(effective_free_bytes: Option<u64>) -> bool {
    matches!(effective_free_bytes, Some(free) if free < STARTUP_WARN_BYTES)
}

#[inline]
fn should_emit_pressure_change_alert(previous: DiskPressure, current: DiskPressure) -> bool {
    previous != current
}

pub fn start(config: &Config) {
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
        && let Some(stale) = worker.take()
    {
        let _ = stale.join();
    }
    if worker.is_none() {
        // Memory protection is independent of the disk-monitor switch. Seed
        // both enabled probes before returning so admission control can see
        // pressure even before the new thread has been scheduled.
        let memory_pressure = mcp_agent_mail_core::memory::sample_and_record(config).pressure;
        let disk_pressure = if config.disk_space_monitor_enabled {
            let sample = mcp_agent_mail_core::disk::sample_and_record(config);
            if should_emit_startup_warning(sample.effective_free_bytes) {
                tracing::warn!(
                    free_bytes = sample.effective_free_bytes,
                    pressure = sample.pressure.label(),
                    "low disk space detected (startup warning threshold)"
                );
            }
            sample.pressure
        } else {
            DiskPressure::Ok
        };
        let config = config.clone();
        SHUTDOWN.store(false, Ordering::Release);
        match std::thread::Builder::new()
            .name("disk-monitor".into())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || monitor_loop(&config, disk_pressure, memory_pressure))
        {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                tracing::warn!(
                    error = %err,
                    "failed to spawn resource monitor worker; continuing without background disk or memory scans"
                );
                return;
            }
        }
    }
    drop(worker);
}

pub fn shutdown() {
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Serialize the flag with start() so a concurrent start cannot clear the
    // stop request while shutdown holds the worker and waits for it to exit.
    SHUTDOWN.store(true, Ordering::Release);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn monitor_loop(
    config: &Config,
    mut last_pressure: DiskPressure,
    mut last_memory_pressure: MemoryPressure,
) {
    let mut schedule = MonitorSchedule::new(config);
    let started = Instant::now();
    tracing::info!(
        disk_enabled = schedule.disk_enabled,
        disk_interval_secs = schedule.disk_interval.as_secs(),
        memory_interval_secs = MEMORY_SAMPLE_INTERVAL.as_secs(),
        "resource monitor worker started"
    );

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("resource monitor worker shutting down");
            return;
        }
        // One-second wakeups retain prompt shutdown without tying the memory
        // cadence to a potentially very large configured disk interval.
        std::thread::sleep(Duration::from_secs(1));
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("resource monitor worker shutting down");
            return;
        }

        let (disk_due, memory_due) = schedule.due(started.elapsed());
        // Sample RSS first when both probes are due. The schedules share a
        // worker, so slow filesystem probes can still delay a later sample.
        if memory_due {
            let sample = mcp_agent_mail_core::memory::sample_and_record(config);
            if last_memory_pressure != sample.pressure {
                tracing::info!(
                    from = last_memory_pressure.label(),
                    to = sample.pressure.label(),
                    rss_bytes = sample.rss_bytes,
                    "memory pressure level changed"
                );
            }
            last_memory_pressure = sample.pressure;
        }

        if disk_due && !SHUTDOWN.load(Ordering::Acquire) {
            let sample = mcp_agent_mail_core::disk::sample_and_record(config);
            if should_emit_pressure_change_alert(last_pressure, sample.pressure) {
                tracing::warn!(
                    from = last_pressure.label(),
                    to = sample.pressure.label(),
                    storage_free_bytes = sample.storage_free_bytes,
                    db_free_bytes = sample.db_free_bytes,
                    effective_free_bytes = sample.effective_free_bytes,
                    "disk pressure level changed"
                );
            }
            last_pressure = sample.pressure;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sampling_remains_active_when_disk_monitoring_is_disabled() {
        let config = Config {
            disk_space_monitor_enabled: false,
            ..Config::default()
        };
        let mut schedule = MonitorSchedule::new(&config);
        assert_eq!(schedule.due(Duration::ZERO), (false, false));
        assert_eq!(schedule.due(Duration::from_secs(4)), (false, false));
        assert_eq!(schedule.due(Duration::from_secs(5)), (false, true));
        assert_eq!(schedule.due(Duration::from_secs(10)), (false, true));
        assert_eq!(schedule.due(Duration::from_secs(3600)), (false, true));
    }

    #[test]
    fn memory_cadence_is_independent_of_configured_disk_interval() {
        for disk_interval in [0, 5, 60, 3600, u64::MAX] {
            let config = Config {
                disk_space_monitor_enabled: true,
                disk_space_check_interval_seconds: disk_interval,
                ..Config::default()
            };
            let mut schedule = MonitorSchedule::new(&config);
            for seconds in 1..=60 {
                let (_, memory_due) = schedule.due(Duration::from_secs(seconds));
                assert_eq!(
                    memory_due,
                    seconds % 5 == 0,
                    "disk interval {disk_interval}"
                );
            }
        }
    }

    #[test]
    fn faster_memory_sampling_does_not_increase_disk_probe_frequency() {
        let config = Config {
            disk_space_monitor_enabled: true,
            disk_space_check_interval_seconds: 60,
            ..Config::default()
        };
        let mut schedule = MonitorSchedule::new(&config);
        for seconds in 1..=120 {
            let (disk_due, memory_due) = schedule.due(Duration::from_secs(seconds));
            assert_eq!(disk_due, seconds % 60 == 0);
            assert_eq!(memory_due, seconds % 5 == 0);
        }
    }

    #[test]
    fn delayed_probes_coalesce_without_catch_up_bursts() {
        let config = Config {
            disk_space_monitor_enabled: true,
            disk_space_check_interval_seconds: 60,
            ..Config::default()
        };
        let mut schedule = MonitorSchedule::new(&config);
        assert_eq!(schedule.due(Duration::from_secs(245)), (true, true));
        assert_eq!(schedule.due(Duration::from_secs(245)), (false, false));
        assert_eq!(schedule.due(Duration::from_secs(249)), (false, false));
        assert_eq!(schedule.due(Duration::from_secs(250)), (false, true));
        assert_eq!(schedule.due(Duration::from_secs(305)), (true, true));
    }

    #[test]
    fn memory_metrics_still_sample_when_all_pressure_thresholds_are_disabled() {
        let config = Config {
            disk_space_monitor_enabled: false,
            memory_warning_mb: 0,
            memory_critical_mb: 0,
            memory_fatal_mb: 0,
            ..Config::default()
        };
        let mut schedule = MonitorSchedule::new(&config);
        assert_eq!(schedule.due(MEMORY_SAMPLE_INTERVAL), (false, true));
    }

    #[test]
    fn monitor_interval_seconds_enforces_minimum() {
        assert_eq!(monitor_interval_seconds(0), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(1), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(4), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(5), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(7), Duration::from_secs(7));
    }

    #[test]
    fn startup_warning_threshold_behavior() {
        assert!(should_emit_startup_warning(Some(STARTUP_WARN_BYTES - 1)));
        assert!(!should_emit_startup_warning(Some(STARTUP_WARN_BYTES)));
        assert!(!should_emit_startup_warning(Some(STARTUP_WARN_BYTES + 1)));
        assert!(!should_emit_startup_warning(None));
    }

    #[test]
    fn startup_warning_zero_free_space_triggers() {
        assert!(should_emit_startup_warning(Some(0)));
    }

    #[test]
    fn pressure_change_alert_only_when_level_changes() {
        assert!(!should_emit_pressure_change_alert(
            DiskPressure::Ok,
            DiskPressure::Ok
        ));
        assert!(!should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Ok,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Critical,
            DiskPressure::Fatal
        ));
    }

    #[test]
    fn warning_to_critical_transition_alerts() {
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Critical
        ));
    }

    #[test]
    fn rapid_fluctuation_alerts_every_transition() {
        let sequence = [
            DiskPressure::Ok,
            DiskPressure::Warning,
            DiskPressure::Critical,
            DiskPressure::Warning,
            DiskPressure::Warning,
            DiskPressure::Ok,
        ];
        let transitions: Vec<bool> = sequence
            .windows(2)
            .map(|window| should_emit_pressure_change_alert(window[0], window[1]))
            .collect();
        assert_eq!(transitions, vec![true, true, true, false, true]);
    }

    #[test]
    fn all_disk_pressure_variants_same_level_no_alert() {
        let all = [
            DiskPressure::Ok,
            DiskPressure::Warning,
            DiskPressure::Critical,
            DiskPressure::Fatal,
        ];
        for p in &all {
            assert!(
                !should_emit_pressure_change_alert(*p, *p),
                "same-level {p:?} should not alert"
            );
        }
    }

    #[test]
    fn recovery_transitions_all_alerted() {
        // Fatal → Critical → Warning → Ok: each step is a recovery alert.
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Fatal,
            DiskPressure::Critical
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Critical,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Ok
        ));
        // Big jumps also alert.
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Fatal,
            DiskPressure::Ok
        ));
    }

    #[test]
    fn monitor_interval_large_value_preserved() {
        assert_eq!(
            monitor_interval_seconds(u64::MAX),
            Duration::from_secs(u64::MAX)
        );
        assert_eq!(monitor_interval_seconds(3600), Duration::from_hours(1));
    }

    #[test]
    fn startup_warning_u64_max_no_warning() {
        // Very large free space should never trigger a warning.
        assert!(!should_emit_startup_warning(Some(u64::MAX)));
    }

    #[test]
    fn startup_warning_handles_unmounted_storage() {
        // When storage paths don't exist, normalize_probe_path falls back to
        // the root "/" (or "." on some systems) which still provides valid
        // disk stats. The system gracefully degrades rather than failing.
        let mut config = Config::from_env();
        config.storage_root =
            std::path::PathBuf::from("/definitely/nonexistent/mcp-agent-mail-root");
        config.database_url = "sqlite:///definitely/nonexistent/mcp-agent-mail.sqlite3".to_string();

        let sample = mcp_agent_mail_core::disk::sample_and_record(&config);

        // The fallback path mechanism means we still get a disk sample from
        // an existing parent directory (usually "/" or ".").
        // This is intentional: always provide best-effort monitoring.
        assert!(
            sample.effective_free_bytes.is_some(),
            "fallback probe paths should still produce disk stats"
        );

        // Verify the fallback doesn't erroneously trigger warnings when there's
        // plenty of disk space (which "/" typically has).
        // This test may be flaky on systems with <1GB free space.
        if sample.effective_free_bytes.unwrap_or(0) >= STARTUP_WARN_BYTES {
            assert!(
                !should_emit_startup_warning(sample.effective_free_bytes),
                "fallback with sufficient space should not trigger warning"
            );
        }
    }
}
