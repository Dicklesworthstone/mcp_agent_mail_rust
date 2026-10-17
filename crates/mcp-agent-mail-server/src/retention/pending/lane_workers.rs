//! Independently owned execution lanes for durable closeout recovery.
//!
//! A blocked acknowledgement journal, release archive, or lane-local pool must
//! not prevent the other kind of closeout from running. Each lane retains one
//! owned thread and a private cancellation flag. Stop every lane before joining
//! any of them; never detach a still-running incarnation to start a replacement.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RESTART_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lane {
    Acknowledgements,
    Releases,
}

impl Lane {
    const fn index(self) -> usize {
        match self {
            Self::Acknowledgements => 0,
            Self::Releases => 1,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Acknowledgements => "acknowledgement",
            Self::Releases => "release",
        }
    }

    const fn thread_name(self) -> &'static str {
        match self {
            Self::Acknowledgements => "degraded-ack-replay",
            Self::Releases => "degraded-release-replay",
        }
    }
}

#[derive(Default)]
pub(super) struct Workers {
    slots: [Option<Worker>; 2],
}

impl Workers {
    /// Start only an absent or finished lane. Callers serialize this method
    /// with shutdown; lane jobs must never acquire that lifecycle mutex.
    pub(super) fn start(
        &mut self,
        lane: Lane,
        run: impl FnMut(&AtomicBool) + Send + 'static,
    ) -> std::io::Result<bool> {
        let slot = &mut self.slots[lane.index()];
        if slot.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Ok(false);
        }
        // Join the finished incarnation before constructing its successor.
        // Its stop flag is never cleared or shared with the new worker.
        drop(slot.take());
        *slot = Some(Worker::spawn(lane, RESTART_BACKOFF, run)?);
        Ok(true)
    }

    pub(super) fn shutdown(&mut self) {
        // Signal both before either join. A slow operation in one lane must
        // not delay delivery of the other lane's cancellation request.
        for worker in self.slots.iter().flatten() {
            worker.request_stop();
        }
        for slot in &mut self.slots {
            drop(slot.take());
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(
        lane: Lane,
        restart_backoff: Duration,
        mut run: impl FnMut(&AtomicBool) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let requested = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(lane.thread_name().to_string())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || {
                while !requested.load(Ordering::Acquire) {
                    // Unwind drops the lane's pool, pinned journal handles and
                    // disposable cursors before the next invocation starts.
                    // The durable journal remains the replay authority.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(&requested);
                    }));
                    if requested.load(Ordering::Acquire) {
                        return;
                    }
                    tracing::warn!(
                        kind = lane.label(),
                        panicked = result.is_err(),
                        "durable closeout lane exited unexpectedly; retrying after backoff"
                    );
                    if sleep_until_stop(&requested, restart_backoff) {
                        return;
                    }
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Cooperative sleep only: this does not interrupt filesystem, database, Git,
/// or logging calls. Work is always joined, even when an operation is slow.
pub(super) fn sleep_until_stop(stop: &AtomicBool, duration: Duration) -> bool {
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let remaining = duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(STOP_POLL_INTERVAL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn either_lane_can_progress_while_the_other_is_blocked() {
        for blocked in [Lane::Acknowledgements, Lane::Releases] {
            let progressing = match blocked {
                Lane::Acknowledgements => Lane::Releases,
                Lane::Releases => Lane::Acknowledgements,
            };
            let mut workers = Workers::default();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (progress_tx, progress_rx) = mpsc::channel();
            workers.start(blocked, move |stop| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                stop.store(true, Ordering::Release);
            }).unwrap();
            entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            workers.start(progressing, move |stop| {
                progress_tx.send(()).unwrap();
                stop.store(true, Ordering::Release);
            }).unwrap();
            let progressed_while_blocked = progress_rx.recv_timeout(TEST_TIMEOUT);
            let blocked_still_running = !workers.slots[blocked.index()].as_ref().unwrap().is_finished();
            let _ = release_tx.send(());
            workers.shutdown();
            assert!(progressed_while_blocked.is_ok());
            assert!(blocked_still_running);
            assert!(workers.slots.iter().all(Option::is_none));
        }
    }

    #[test]
    fn shutdown_signals_both_lanes_before_joining_either() {
        let mut workers = Workers::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let (second_stopped_tx, second_stopped_rx) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();
        workers.start(Lane::Acknowledgements, move |stop| {
            entered_tx.send(()).unwrap();
            while !sleep_until_stop(stop, Duration::from_millis(1)) {}
            // This join depends on the second lane receiving its stop request.
            observed_tx.send(second_stopped_rx.recv_timeout(TEST_TIMEOUT).is_ok()).unwrap();
        }).unwrap();
        workers.start(Lane::Releases, move |stop| {
            second_entered_tx.send(()).unwrap();
            while !sleep_until_stop(stop, Duration::from_millis(1)) {}
            second_stopped_tx.send(()).unwrap();
        }).unwrap();
        entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        second_entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        workers.shutdown();
        assert!(observed_rx.recv_timeout(TEST_TIMEOUT).unwrap());
    }

    #[test]
    fn repeated_start_does_not_duplicate_a_live_lane() {
        let mut workers = Workers::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        workers.start(Lane::Acknowledgements, move |stop| {
            entered_tx.send(()).unwrap();
            sleep_until_stop(stop, Duration::from_secs(3600));
        }).unwrap();
        entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let duplicate_ran = Arc::new(AtomicBool::new(false));
        let duplicate = Arc::clone(&duplicate_ran);
        assert!(!workers.start(Lane::Acknowledgements, move |_| {
            duplicate.store(true, Ordering::Release);
        }).unwrap());
        workers.shutdown();
        assert!(!duplicate_ran.load(Ordering::Acquire));
    }

    #[test]
    fn a_finished_lane_restarts_with_a_new_cancellation_flag() {
        let mut workers = Workers::default();
        workers.start(Lane::Acknowledgements, |stop| {
            stop.store(true, Ordering::Release);
        }).unwrap();
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !workers.slots[0].as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let previous_stop = Arc::clone(&workers.slots[0].as_ref().unwrap().stop);
        let (entered_tx, entered_rx) = mpsc::channel();
        assert!(workers.start(Lane::Acknowledgements, move |stop| {
            entered_tx.send(()).unwrap();
            sleep_until_stop(stop, Duration::from_secs(3600));
        }).unwrap());
        entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let successor = workers.slots[0].as_ref().unwrap();
        assert!(!Arc::ptr_eq(&previous_stop, &successor.stop));
        assert!(previous_stop.load(Ordering::Acquire));
        assert!(!successor.stop.load(Ordering::Acquire));
        workers.shutdown();
    }

    #[test]
    fn panic_and_unexpected_return_restart_with_backoff() {
        for panic_first in [false, true] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&attempts);
            let (recovered_tx, recovered_rx) = mpsc::channel();
            let started = Instant::now();
            let delay = Duration::from_millis(20);
            let worker = Worker::spawn(Lane::Releases, delay, move |stop| {
                if counted.fetch_add(1, Ordering::AcqRel) == 0 {
                    assert!(!panic_first, "test lane panic");
                    return;
                }
                recovered_tx.send(()).unwrap();
                stop.store(true, Ordering::Release);
            }).unwrap();
            let recovered = recovered_rx.recv_timeout(TEST_TIMEOUT);
            drop(worker);
            assert!(recovered.is_ok());
            assert_eq!(attempts.load(Ordering::Acquire), 2);
            assert!(started.elapsed() >= delay);
        }
    }

    #[test]
    fn dropping_workers_joins_and_destroys_jobs_despite_long_sleep() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let destroyed = Arc::new(AtomicBool::new(false));
        let owner = Dropped(Arc::clone(&destroyed));
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut workers = Workers::default();
        workers.start(Lane::Acknowledgements, move |stop| {
            let _retained = &owner;
            entered_tx.send(()).unwrap();
            sleep_until_stop(stop, Duration::from_secs(3600));
        }).unwrap();
        entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        drop(workers);
        assert!(destroyed.load(Ordering::Acquire));
        assert!(sleep_until_stop(&AtomicBool::new(true), Duration::ZERO));
        assert!(!sleep_until_stop(&AtomicBool::new(false), Duration::ZERO));
    }
}
