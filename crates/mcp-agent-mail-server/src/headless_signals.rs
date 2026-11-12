//! Scoped Unix shutdown delivery without a pipe or dispatcher thread.
//!
//! GH#379: the global asynchronous signal dispatcher can die after a pipe
//! error while its subscribers remain pending forever. The headless supervisor
//! already polls on its own thread, so a signal-safe atomic store is sufficient.
//! Keep the handlers alive across startup, serving, and the bounded drain.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct ShutdownSignals {
    requested: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}

impl ShutdownSignals {
    pub(super) fn new() -> io::Result<Self> {
        let mut signals = Self {
            requested: Arc::new(AtomicUsize::new(0)),
            registrations: Vec::with_capacity(2),
        };
        for (signal, value) in [
            (signal_hook::consts::SIGTERM, 1),
            (signal_hook::consts::SIGINT, 2),
        ] {
            signals
                .registrations
                .push(signal_hook::flag::register_usize(
                    signal,
                    Arc::clone(&signals.requested),
                    value,
                )?);
        }
        // If a later registration fails, Drop unregisters the earlier ones.
        Ok(signals)
    }

    pub(super) fn requested(&self) -> Option<&'static str> {
        match self.requested.load(Ordering::Acquire) {
            1 => Some("SIGTERM"),
            2 => Some("SIGINT"),
            _ => None,
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses real process signals. Isolate them from the other server
    // tests and from any signal listeners installed by the test harness.
    fn with_signal_case(run: impl FnOnce(i32)) {
        const CASE: &str = "AM_TEST_HEADLESS_SHUTDOWN_SIGNAL";
        if let Ok(signal) = std::env::var(CASE) {
            run(signal.parse().unwrap());
            return;
        }
        let name = std::thread::current().name().unwrap().to_string();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CASE, signal.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "signal {signal} child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn expected_name(signal: i32) -> &'static str {
        if signal == signal_hook::consts::SIGTERM {
            "SIGTERM"
        } else {
            "SIGINT"
        }
    }

    #[test]
    fn delivery_survives_startup_and_repeated_polls_without_a_dispatcher() {
        with_signal_case(|signal| {
            let signals = ShutdownSignals::new().unwrap();
            assert_eq!(signals.requested(), None);
            signal_hook::low_level::raise(signal).unwrap();
            // Model time spent in runtime/mailbox startup before the first
            // supervisor poll; no task needs to receive a transient wakeup.
            std::thread::sleep(std::time::Duration::from_millis(20));
            for _ in 0..3 {
                assert_eq!(signals.requested(), Some(expected_name(signal)));
            }
        });
    }

    #[test]
    fn delivery_survives_an_independent_signal_dispatcher_exiting() {
        with_signal_case(|signal| {
            let mut dispatcher = signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGINT,
            ])
            .unwrap();
            let handle = dispatcher.handle();
            let thread = std::thread::spawn(move || for _ in dispatcher.forever() {});
            let signals = ShutdownSignals::new().unwrap();
            handle.close();
            thread.join().unwrap();

            signal_hook::low_level::raise(signal).unwrap();
            assert_eq!(signals.requested(), Some(expected_name(signal)));
        });
    }

    #[test]
    fn dropping_a_guard_unregisters_only_its_own_handlers() {
        with_signal_case(|signal| {
            let independent = Arc::new(AtomicUsize::new(0));
            let registration =
                signal_hook::flag::register_usize(signal, Arc::clone(&independent), 7).unwrap();
            let first = ShutdownSignals::new().unwrap();
            let old_flag = Arc::clone(&first.requested);
            drop(first);
            signal_hook::low_level::raise(signal).unwrap();
            assert_eq!(old_flag.load(Ordering::Acquire), 0);
            assert_eq!(independent.load(Ordering::Acquire), 7);

            let next = ShutdownSignals::new().unwrap();
            assert_eq!(next.requested(), None);
            signal_hook::low_level::raise(signal).unwrap();
            assert_eq!(next.requested(), Some(expected_name(signal)));
            assert_eq!(old_flag.load(Ordering::Acquire), 0);
            signal_hook::low_level::unregister(registration);
        });
    }
}
