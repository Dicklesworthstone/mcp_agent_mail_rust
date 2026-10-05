//! Background monitoring of resource pressure and unfinished lock waits.
//!
//! Memory has its own sampling cadence and remains active when disk probes are
//! disabled. Both feed `health_check` and `resource://tooling/metrics_core`.
//! Long ordered-lock waits also emit bounded holder/waiter evidence without a
//! client diagnostic request. These observations do not prove a deadlock or
//! replace the archive queue-progress health verdict.
//! The lock sampler has its own joinable thread: a blocked filesystem or memory
//! probe must not suppress the evidence needed to diagnose that probe.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::DiskPressure;
use mcp_agent_mail_core::lock_order::{LockActivity, LockActivitySnapshot, lock_activity_snapshot};
use mcp_agent_mail_core::memory::MemoryPressure;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
static LOCK_WORKER: std::sync::LazyLock<Mutex<Option<LockWatchdogWorker>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
const STARTUP_WARN_BYTES: u64 = 1024 * 1024 * 1024; // 1GiB
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const LOCK_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const LOCK_WAIT_THRESHOLD: Duration = Duration::from_secs(30);
const LOCK_WAIT_REMINDER_INTERVAL: Duration = Duration::from_secs(60);
const MAX_TRACKED_LOCK_WAITS: usize = 128;
const MAX_LOCK_REPORTS_PER_SAMPLE: usize = 4;

/// Per-instance incidents, independent of resettable acquisition counters.
/// Missing entries in an incomplete sample never certify recovery. Retain only
/// IDs and report times, not growing histories or protected application values.
#[derive(Default)]
struct LockWaitWatchdog {
    reported: BTreeMap<u64, Duration>,
    incomplete_since: Option<Duration>,
    last_coverage_report: Option<Duration>,
}

#[derive(Debug, Default)]
struct LockWaitUpdate<'a> {
    stalled: Vec<&'a LockActivity>,
    cleared: Vec<u64>,
    deferred_reports: usize,
    report_incomplete: bool,
}

impl LockWaitWatchdog {
    fn observe<'a>(
        &mut self,
        elapsed: Duration,
        snapshot: &'a LockActivitySnapshot,
        threshold: Duration,
    ) -> LockWaitUpdate<'a> {
        let mut update = LockWaitUpdate::default();
        let complete = snapshot.is_complete();
        if snapshot.available {
            self.reported.retain(|id, _| {
                let no_waiters = snapshot
                    .locks
                    .iter()
                    .find(|lock| lock.instance_id == *id)
                    .map_or(complete, |lock| lock.waiter_count == 0);
                if no_waiters {
                    update.cleared.push(*id);
                }
                !no_waiters
            });
            for lock in &snapshot.locks {
                if lock.waiter_count == 0
                    || Duration::from_nanos(lock.oldest_observed_wait_ns) < threshold
                {
                    continue;
                }
                let previous = self.reported.get(&lock.instance_id);
                if previous
                    .is_some_and(|last| elapsed.saturating_sub(*last) < LOCK_WAIT_REMINDER_INTERVAL)
                {
                    continue;
                }
                if update.stalled.len() >= MAX_LOCK_REPORTS_PER_SAMPLE
                    || (previous.is_none() && self.reported.len() >= MAX_TRACKED_LOCK_WAITS)
                {
                    update.deferred_reports += 1;
                    continue;
                }
                update.stalled.push(lock);
                self.reported.insert(lock.instance_id, elapsed);
            }
        }

        // A briefly busy metadata mutex is normal, not a liveness incident.
        // Repeatedly unavailable/truncated evidence is reported explicitly;
        // never describe an unobserved instance as recovered or idle.
        if !complete || update.deferred_reports > 0 {
            let since = *self.incomplete_since.get_or_insert(elapsed);
            if elapsed.saturating_sub(since) >= LOCK_WAIT_THRESHOLD
                && self
                    .last_coverage_report
                    .is_none_or(|last| elapsed.saturating_sub(last) >= LOCK_WAIT_REMINDER_INTERVAL)
            {
                update.report_incomplete = true;
                self.last_coverage_report = Some(elapsed);
            }
        } else {
            self.incomplete_since = None;
            self.last_coverage_report = None;
        }
        update
    }

    fn sample(&mut self, elapsed: Duration) {
        // This reads private tracking metadata with try_lock; it never takes
        // the application locks whose waiters we are trying to diagnose.
        let snapshot = lock_activity_snapshot();
        let update = self.observe(elapsed, &snapshot, LOCK_WAIT_THRESHOLD);
        for lock in &update.stalled {
            tracing::error!(
                target: "maintenance",
                event = "ordered_lock_wait_stalled",
                instance_id = lock.instance_id,
                lock_name = %lock.lock_name,
                rank = lock.rank,
                wait_threshold_secs = LOCK_WAIT_THRESHOLD.as_secs(),
                oldest_observed_wait_ns = lock.oldest_observed_wait_ns,
                holder_count = lock.holder_count,
                waiter_count = lock.waiter_count,
                holders = ?lock.holders,
                waiters = ?lock.waiters,
                omitted_participants = lock.omitted_participants,
                coverage_complete = snapshot.is_complete(),
                "ordered lock acquisition remains blocked; holder/waiter evidence is observational, not a deadlock proof"
            );
        }
        if !update.cleared.is_empty() {
            tracing::info!(
                target: "maintenance",
                event = "ordered_lock_wait_cleared",
                instance_ids = ?update.cleared,
                "previously reported ordered lock instances have no observed waiters"
            );
        }
        if update.report_incomplete {
            tracing::warn!(
                target: "maintenance",
                event = "ordered_lock_observation_incomplete",
                available = snapshot.available,
                busy_instances = snapshot.busy_instances,
                omitted_active_instances = snapshot.omitted_active_instances,
                registrations_dropped = snapshot.registrations_dropped,
                deferred_reports = update.deferred_reports,
                tracked_incidents = self.reported.len(),
                "lock observation remains incomplete; missing evidence does not establish recovery"
            );
        }
    }
}

/// Own the sampler and join it on every drop path. Each worker incarnation has
/// its own cancellation flag; restarting cannot revive an older thread.
struct LockWatchdogWorker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LockWatchdogWorker {
    fn spawn(
        interval: Duration,
        sample: impl FnMut(Duration) + Send + 'static,
    ) -> std::io::Result<Self> {
        if interval.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lock sampling interval must be positive",
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("lock-watchdog".to_string())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || run_lock_sampler(&worker_stop, interval, sample))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for LockWatchdogWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_lock_sampler(stop: &AtomicBool, interval: Duration, mut sample: impl FnMut(Duration)) {
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) {
        sample(started.elapsed());
        // Wait after each completed observation; missed intervals never cause
        // a catch-up burst. No database, filesystem, or application lock is
        // involved in this wait. A blocked tracing sink is not interruptible.
        let paused = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let remaining = interval.saturating_sub(paused.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}

/// Start and stop take WORKER before LOCK_WORKER, serializing the lifecycle.
/// The sampler itself takes neither lifecycle mutex.
fn start_lock_watchdog() {
    let mut worker = LOCK_WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker.as_ref().is_some_and(LockWatchdogWorker::is_finished) {
        drop(worker.take());
    }
    if worker.is_none() {
        let mut observations = LockWaitWatchdog::default();
        match LockWatchdogWorker::spawn(LOCK_SAMPLE_INTERVAL, move |elapsed| {
            observations.sample(elapsed);
        }) {
            Ok(started) => *worker = Some(started),
            Err(error) => tracing::warn!(
                %error,
                "failed to start independent lock watchdog; resource monitoring remains enabled"
            ),
        }
    }
}

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

/// Feed the descriptor leak detector (br-kp1in.17) and warn once each time
/// it starts reporting a rising floor. Returns whether growth is reported.
fn sample_descriptors(elapsed: Duration, reported: bool) -> bool {
    let Some(open_fds) = mcp_agent_mail_core::count_open_fds() else {
        return reported;
    };
    mcp_agent_mail_core::record_descriptor_sample(
        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        open_fds,
    );
    let growth = mcp_agent_mail_core::descriptor_floor_growth();
    if let Some(growth) = growth.as_ref().filter(|_| !reported) {
        tracing::warn!(
            open_fds,
            floors = ?growth.floors,
            per_hour = growth.per_hour,
            "open descriptor floor rose in every recent window: possible descriptor leak"
        );
    }
    growth.is_some()
}

pub fn start(config: &Config) {
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Start before synchronous resource probes: even a startup probe can wait
    // on an instrumented lock. Retrying start never duplicates a live sampler.
    start_lock_watchdog();
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
    // Keep reporting while a resource probe is draining. Only after that
    // worker joins do we cancel and join the independent sampler.
    let mut lock_worker = LOCK_WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(lock_worker.take());
}

fn monitor_loop(
    config: &Config,
    mut last_pressure: DiskPressure,
    mut last_memory_pressure: MemoryPressure,
) {
    let mut schedule = MonitorSchedule::new(config);
    let started = Instant::now();
    let mut descriptor_growth_reported = false;
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
            descriptor_growth_reported =
                sample_descriptors(started.elapsed(), descriptor_growth_reported);
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
    use mcp_agent_mail_core::lock_order::{LockAccess, LockParticipant};

    fn waiting_lock(instance_id: u64, seconds: u64) -> LockActivity {
        let participant = LockParticipant {
            thread_id: format!("waiter-{instance_id}"),
            thread_name: Some("blocked-worker".to_string()),
            access: LockAccess::Mutex,
            file: "src/archive.rs".to_string(),
            line: 42,
            column: 9,
            elapsed_ns: seconds.saturating_mul(1_000_000_000),
            labels_truncated: false,
        };
        LockActivity {
            instance_id,
            lock_name: "StorageCommitQueue".to_string(),
            rank: 60,
            holder_count: 1,
            waiter_count: 1,
            oldest_observed_wait_ns: participant.elapsed_ns,
            holders: vec![LockParticipant {
                thread_id: format!("holder-{instance_id}"),
                thread_name: Some("archive-owner".to_string()),
                line: 11,
                ..participant.clone()
            }],
            waiters: vec![participant],
            omitted_participants: 0,
        }
    }

    fn lock_snapshot(locks: Vec<LockActivity>) -> LockActivitySnapshot {
        LockActivitySnapshot {
            available: true,
            locks,
            ..LockActivitySnapshot::default()
        }
    }

    fn observe_waits<'a>(
        watchdog: &mut LockWaitWatchdog,
        snapshot: &'a LockActivitySnapshot,
        seconds: u64,
    ) -> LockWaitUpdate<'a> {
        watchdog.observe(Duration::from_secs(seconds), snapshot, LOCK_WAIT_THRESHOLD)
    }

    #[test]
    fn lock_watchdog_reports_threshold_and_reminders_without_completed_acquisitions() {
        let mut watchdog = LockWaitWatchdog::default();
        let mut snapshot = lock_snapshot(vec![waiting_lock(1, 29)]);
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 0).stalled,
            [] as [&LockActivity; 0]
        );
        snapshot.locks[0] = waiting_lock(1, 30);
        let first = observe_waits(&mut watchdog, &snapshot, 1);
        assert_eq!(first.stalled.len(), 1);
        assert_eq!(first.stalled[0].holders[0].line, 11);
        assert_eq!(first.stalled[0].waiters[0].line, 42);
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 60).stalled,
            [] as [&LockActivity; 0]
        );
        assert_eq!(observe_waits(&mut watchdog, &snapshot, 61).stalled.len(), 1);
        // Wall-clock changes are irrelevant; even an out-of-order supplied
        // elapsed value must not underflow or trigger an immediate reminder.
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 0).stalled,
            [] as [&LockActivity; 0]
        );
    }

    #[test]
    fn lock_watchdog_ignores_long_holds_without_waiters_and_rearms_after_clear() {
        let mut watchdog = LockWaitWatchdog::default();
        let mut snapshot = lock_snapshot(vec![waiting_lock(1, 300)]);
        assert_eq!(observe_waits(&mut watchdog, &snapshot, 0).stalled.len(), 1);
        snapshot.locks[0].waiter_count = 0;
        snapshot.locks[0].oldest_observed_wait_ns = 0;
        snapshot.locks[0].waiters.clear();
        let cleared = observe_waits(&mut watchdog, &snapshot, 5);
        assert_eq!(cleared.cleared, vec![1]);
        assert_eq!(cleared.stalled, [] as [&LockActivity; 0]);
        assert!(watchdog.reported.is_empty());
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 10).cleared,
            [] as [u64; 0]
        );
        snapshot.locks[0] = waiting_lock(1, 30);
        assert_eq!(observe_waits(&mut watchdog, &snapshot, 15).stalled.len(), 1);
    }

    #[test]
    fn lock_watchdog_never_clears_an_unobserved_instance_from_partial_evidence() {
        let initial = lock_snapshot(vec![waiting_lock(1, 30)]);
        let partials = [
            LockActivitySnapshot::default(),
            LockActivitySnapshot {
                busy_instances: 1,
                ..lock_snapshot(vec![])
            },
            LockActivitySnapshot {
                omitted_active_instances: 1,
                ..lock_snapshot(vec![])
            },
            LockActivitySnapshot {
                registrations_dropped: 1,
                ..lock_snapshot(vec![])
            },
        ];
        for partial in partials {
            let mut watchdog = LockWaitWatchdog::default();
            assert_eq!(observe_waits(&mut watchdog, &initial, 0).stalled.len(), 1);
            assert_eq!(
                observe_waits(&mut watchdog, &partial, 5).cleared,
                [] as [u64; 0]
            );
            let unknown = observe_waits(&mut watchdog, &partial, 35);
            assert!(unknown.report_incomplete);
            assert_eq!(unknown.cleared, [] as [u64; 0]);
            assert_eq!(watchdog.reported.len(), 1);
            assert!(!observe_waits(&mut watchdog, &partial, 40).report_incomplete);
            assert!(observe_waits(&mut watchdog, &partial, 95).report_incomplete);
            let empty = lock_snapshot(vec![]);
            assert_eq!(observe_waits(&mut watchdog, &empty, 100).cleared, vec![1]);
        }
    }

    #[test]
    fn lock_watchdog_accepts_observed_clear_despite_unrelated_missing_coverage() {
        let mut watchdog = LockWaitWatchdog::default();
        let mut snapshot = lock_snapshot(vec![waiting_lock(1, 30)]);
        let _ = observe_waits(&mut watchdog, &snapshot, 0);
        snapshot.busy_instances = 1;
        snapshot.locks[0].waiter_count = 0;
        snapshot.locks[0].waiters.clear();
        snapshot.locks[0].oldest_observed_wait_ns = 0;
        assert_eq!(observe_waits(&mut watchdog, &snapshot, 5).cleared, vec![1]);
        // An unavailable sample cannot lend authority even to a populated list.
        snapshot.locks[0] = waiting_lock(2, 30);
        snapshot.available = false;
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 10).stalled,
            [] as [&LockActivity; 0]
        );
    }

    #[test]
    fn lock_watchdog_bounds_reports_and_eventually_reports_each_visible_instance() {
        let snapshot = lock_snapshot((1..=12).map(|id| waiting_lock(id, 30)).collect());
        let mut watchdog = LockWaitWatchdog::default();
        let mut reported = Vec::new();
        for seconds in [0, 5, 10] {
            let update = observe_waits(&mut watchdog, &snapshot, seconds);
            assert_eq!(update.stalled.len(), MAX_LOCK_REPORTS_PER_SAMPLE);
            reported.extend(update.stalled.iter().map(|lock| lock.instance_id));
        }
        assert_eq!(reported, (1..=12).collect::<Vec<_>>());
        assert_eq!(
            observe_waits(&mut watchdog, &snapshot, 15).stalled,
            [] as [&LockActivity; 0]
        );
    }

    #[test]
    fn lock_watchdog_bounds_uncertain_history_without_false_recovery() {
        let mut watchdog = LockWaitWatchdog::default();
        for index in 0..MAX_TRACKED_LOCK_WAITS {
            let id = u64::try_from(index).unwrap();
            let snapshot = LockActivitySnapshot {
                busy_instances: 1,
                ..lock_snapshot(vec![waiting_lock(id, 30)])
            };
            let update = observe_waits(&mut watchdog, &snapshot, 0);
            assert_eq!(update.stalled.len(), 1);
            assert_eq!(update.cleared, [] as [u64; 0]);
        }
        let mut snapshot = LockActivitySnapshot {
            busy_instances: 1,
            ..lock_snapshot(vec![waiting_lock(10_000, 30)])
        };
        let capped = observe_waits(&mut watchdog, &snapshot, 30);
        assert_eq!(capped.stalled, [] as [&LockActivity; 0]);
        assert_eq!(capped.cleared, [] as [u64; 0]);
        assert_eq!(capped.deferred_reports, 1);
        assert!(capped.report_incomplete);
        assert_eq!(watchdog.reported.len(), MAX_TRACKED_LOCK_WAITS);
        snapshot.busy_instances = 0;
        let recovered = observe_waits(&mut watchdog, &snapshot, 35);
        assert_eq!(recovered.cleared.len(), MAX_TRACKED_LOCK_WAITS);
        assert_eq!(recovered.stalled.len(), 1);
        assert_eq!(watchdog.reported.len(), 1);
    }

    #[test]
    fn lock_watchdog_observes_a_real_wait_without_acquiring_the_blocked_mutex() {
        use mcp_agent_mail_core::{LockLevel, OrderedMutex};
        use std::sync::{Arc, mpsc};

        let lock = Arc::new(OrderedMutex::new(LockLevel::StorageCommitQueue, ()));
        let guard = lock.lock();
        let peer = Arc::clone(&lock);
        let (ready, receive_ready) = mpsc::channel();
        let child = std::thread::Builder::new()
            .name("lock-watchdog-contender".to_string())
            .spawn(move || {
                ready.send(()).unwrap();
                let _guard = peer.lock();
            })
            .unwrap();
        let ready = receive_ready.recv_timeout(Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        let observed = loop {
            let snapshot = lock_activity_snapshot();
            if let Some(activity) = snapshot.locks.into_iter().find(|activity| {
                activity
                    .waiters
                    .iter()
                    .any(|waiter| waiter.thread_name.as_deref() == Some("lock-watchdog-contender"))
            }) {
                break Some(activity);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::yield_now();
        };
        // Always release and join before asserting, including observation
        // timeout, so a failed test cannot leave a permanently blocked child.
        drop(guard);
        child.join().unwrap();
        ready.unwrap();
        let observed = observed.expect("real contender must appear before acquisition completes");
        assert_eq!(observed.holder_count, 1);
        assert_eq!(observed.waiter_count, 1);
        assert_ne!(observed.holders, [] as [LockParticipant; 0]);
        let id = observed.instance_id;
        let snapshot = lock_snapshot(vec![observed]);
        let mut watchdog = LockWaitWatchdog::default();
        let blocked = watchdog.observe(Duration::ZERO, &snapshot, Duration::ZERO);
        assert_eq!(blocked.stalled.len(), 1);
        let _held_again = lock.lock();
        let after = lock_activity_snapshot();
        assert!(
            after
                .locks
                .iter()
                .any(|activity| { activity.instance_id == id && activity.waiter_count == 0 })
        );
        let cleared = watchdog.observe(Duration::from_secs(5), &after, Duration::ZERO);
        assert_eq!(cleared.cleared, vec![id]);
    }

    #[test]
    fn independent_lock_sampler_progresses_and_stops_while_a_probe_is_blocked() {
        use mcp_agent_mail_core::{LockLevel, OrderedMutex};
        use std::sync::mpsc;

        let lock = Arc::new(OrderedMutex::new(LockLevel::StorageCommitQueue, ()));
        let guard = lock.lock();
        let acquired = Arc::new(AtomicBool::new(false));
        let peer = Arc::clone(&lock);
        let peer_acquired = Arc::clone(&acquired);
        let (ready, receive_ready) = mpsc::channel();
        let probe = std::thread::Builder::new()
            .name("blocked-resource-probe".to_string())
            .spawn(move || {
                ready.send(()).unwrap();
                let _guard = peer.lock();
                peer_acquired.store(true, Ordering::Release);
            })
            .unwrap();
        let ready = receive_ready.recv_timeout(Duration::from_secs(5));
        let (evidence, receive_evidence) = mpsc::sync_channel(1);
        let sampler = LockWatchdogWorker::spawn(Duration::from_millis(5), move |_| {
            let snapshot = lock_activity_snapshot();
            if let Some(activity) = snapshot.locks.into_iter().find(|activity| {
                activity
                    .waiters
                    .iter()
                    .any(|waiter| waiter.thread_name.as_deref() == Some("blocked-resource-probe"))
            }) {
                // A slow test receiver must not block sampler cancellation.
                let _ = evidence.try_send(activity);
            }
        })
        .unwrap();
        let observed = receive_evidence.recv_timeout(Duration::from_secs(5));
        // The sampler joins while the probe is still blocked; neither its
        // reads nor its cancellation require the protected application lock.
        drop(sampler);
        let still_blocked = !acquired.load(Ordering::Acquire);
        drop(guard);
        probe.join().unwrap();
        ready.unwrap();
        let observed = observed.expect("independent sampler must observe the blocked probe");
        assert!(still_blocked);
        assert_eq!(observed.holder_count, 1);
        assert_eq!(observed.waiter_count, 1);
        assert_ne!(observed.holders, [] as [LockParticipant; 0]);
        assert!(acquired.load(Ordering::Acquire));
    }

    #[test]
    fn lock_sampler_drop_joins_without_waiting_for_the_next_sampling_interval() {
        use std::sync::mpsc;

        let (sampled, receive_sample) = mpsc::channel();
        let worker = LockWatchdogWorker::spawn(Duration::from_secs(3600), move |elapsed| {
            let _ = sampled.send(elapsed);
        })
        .unwrap();
        let first = receive_sample.recv_timeout(Duration::from_secs(5));
        drop(worker);
        assert!(
            first.is_ok(),
            "the first sample does not wait for the interval"
        );
        // Sender destruction proves the sampling closure and its thread have
        // actually exited, rather than merely detaching a sleeping worker.
        assert!(matches!(
            receive_sample.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn lock_sampler_refuses_zero_interval_and_honors_preexisting_cancellation() {
        let error = LockWatchdogWorker::spawn(Duration::ZERO, |_| {
            panic!("invalid interval must not start a worker");
        })
        .err()
        .expect("zero interval must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let stop = AtomicBool::new(true);
        let mut samples = 0;
        run_lock_sampler(&stop, LOCK_SAMPLE_INTERVAL, |_| samples += 1);
        assert_eq!(samples, 0);
    }

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
