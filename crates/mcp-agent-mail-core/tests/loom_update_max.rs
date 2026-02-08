//! Loom model-check for the `update_max` CAS loop (`lock_order.rs`).
//!
//! The production `update_max` function uses a `compare_exchange_weak` loop to
//! atomically update a shared maximum. Under concurrent execution, a naive
//! approach could lose updates or loop forever.
//!
//! # Invariants
//!
//! 1. **Convergence**: after all threads complete, the target holds `max(all candidates)`.
//! 2. **Termination**: the CAS loop always terminates (no infinite retries).
//!
//! # Reproduction
//!
//! Loom exhaustively explores all interleavings. Set `LOOM_LOG=trace` to see
//! the interleaving that triggers a failure.
//!
//! # Running
//!
//! ```sh
//! cargo test --features loom-tests -p mcp-agent-mail-core --test loom_update_max
//! ```

#![cfg(feature = "loom-tests")]

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Reimplementation of `lock_order::update_max` using loom atomics.
///
/// This is an exact copy of the production algorithm; only the import
/// path differs (`loom::sync::atomic` vs `std::sync::atomic`).
fn update_max(target: &AtomicU64, candidate: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while candidate > current {
        match target.compare_exchange_weak(
            current,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

#[test]
fn three_threads_converge_to_maximum() {
    // Invariant: final value == max(10, 20, 30) == 30 under ALL interleavings.
    loom::model(|| {
        let target = Arc::new(AtomicU64::new(0));

        let t1 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 10))
        };
        let t2 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 30))
        };
        let t3 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 20))
        };

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        let final_val = target.load(Ordering::Relaxed);
        assert_eq!(final_val, 30, "update_max must converge to maximum");
    });
}

#[test]
fn equal_values_terminate_without_infinite_loop() {
    // Invariant: update_max terminates and produces correct result even when
    // all threads supply the same candidate value.
    loom::model(|| {
        let target = Arc::new(AtomicU64::new(0));

        let t1 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 42))
        };
        let t2 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 42))
        };

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(
            target.load(Ordering::Relaxed),
            42,
            "equal candidates must produce correct result"
        );
    });
}

#[test]
fn ascending_values_from_zero() {
    // Invariant: when two threads race with values where one strictly dominates,
    // the maximum always wins regardless of execution order.
    loom::model(|| {
        let target = Arc::new(AtomicU64::new(5));

        let t1 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 3)) // below initial: no-op
        };
        let t2 = {
            let t = Arc::clone(&target);
            thread::spawn(move || update_max(&t, 100)) // above initial: must win
        };

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(
            target.load(Ordering::Relaxed),
            100,
            "highest candidate must win even when racing with below-initial value"
        );
    });
}
