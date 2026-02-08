//! Loom model-check for the `CoalesceMap` leader/joiner slot protocol.
//!
//! The production `Slot` type coordinates a leader thread (which executes the
//! actual work) with joiner threads (which block until the leader broadcasts
//! the result). This uses `Mutex<SlotState>` + `Condvar` — a pattern prone to
//! subtle races if notification/state-update ordering is wrong.
//!
//! # Invariants
//!
//! 1. **No lost updates**: a joiner waiting on a slot always sees the leader's
//!    value after `complete()` is called, even if `complete()` races with `wait()`.
//! 2. **No deadlocks**: loom will detect any interleaving that blocks forever.
//! 3. **No panics**: all paths through the protocol terminate cleanly.
//!
//! # Reproduction
//!
//! Loom exhaustively explores all interleavings. Set `LOOM_LOG=trace` to see
//! the exact schedule that triggers a failure.
//!
//! # Running
//!
//! ```sh
//! cargo test --features loom-tests -p mcp-agent-mail-db --test loom_coalesce
//! ```

#![cfg(feature = "loom-tests")]

use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

// ---------------------------------------------------------------------------
// Reimplementation of the Slot protocol using loom types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum SlotState {
    Pending,
    Ready(i32),
    Failed(String),
}

struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

impl Slot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Pending),
            done: Condvar::new(),
        }
    }

    /// Leader broadcasts a successful result.
    fn complete_ok(&self, value: i32) {
        let mut state = self.state.lock().unwrap();
        *state = SlotState::Ready(value);
        drop(state); // Release lock before notify (matches production code).
        self.done.notify_all();
    }

    /// Leader broadcasts an error.
    fn complete_err(&self, msg: String) {
        let mut state = self.state.lock().unwrap();
        *state = SlotState::Failed(msg);
        drop(state);
        self.done.notify_all();
    }

    /// Joiner blocks until the leader completes (no timeout in loom version).
    fn wait_for_result(&self) -> Result<i32, String> {
        let mut guard = self.state.lock().unwrap();
        while *guard == SlotState::Pending {
            guard = self.done.wait(guard).unwrap();
        }
        match &*guard {
            SlotState::Ready(v) => Ok(*v),
            SlotState::Failed(msg) => Err(msg.clone()),
            SlotState::Pending => unreachable!("exited wait loop with Pending state"),
        }
    }
}

// ---------------------------------------------------------------------------
// Loom tests
// ---------------------------------------------------------------------------

#[test]
fn leader_complete_ok_joiner_receives_value() {
    // Invariant: the joiner always receives exactly the value the leader set,
    // regardless of whether complete_ok() runs before or after wait_for_result().
    loom::model(|| {
        let slot = Arc::new(Slot::new());

        let joiner_slot = Arc::clone(&slot);
        let joiner = thread::spawn(move || joiner_slot.wait_for_result());

        // Leader completes with value 42.
        slot.complete_ok(42);

        let result = joiner.join().unwrap();
        assert_eq!(result, Ok(42), "joiner must receive leader's value");
    });
}

#[test]
fn leader_complete_err_joiner_receives_error() {
    // Invariant: when the leader signals failure, joiners see the error message.
    loom::model(|| {
        let slot = Arc::new(Slot::new());

        let joiner_slot = Arc::clone(&slot);
        let joiner = thread::spawn(move || joiner_slot.wait_for_result());

        slot.complete_err("boom".to_string());

        let result = joiner.join().unwrap();
        assert_eq!(
            result,
            Err("boom".to_string()),
            "joiner must see leader's error"
        );
    });
}

#[test]
fn two_joiners_both_receive_same_value() {
    // Invariant: all joiners see the same value, no lost notifications.
    loom::model(|| {
        let slot = Arc::new(Slot::new());

        let j1_slot = Arc::clone(&slot);
        let j1 = thread::spawn(move || j1_slot.wait_for_result());

        let j2_slot = Arc::clone(&slot);
        let j2 = thread::spawn(move || j2_slot.wait_for_result());

        slot.complete_ok(99);

        let r1 = j1.join().unwrap();
        let r2 = j2.join().unwrap();

        assert_eq!(r1, Ok(99), "joiner 1 must receive leader's value");
        assert_eq!(r2, Ok(99), "joiner 2 must receive leader's value");
    });
}
