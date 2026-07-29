//! Shared test helpers for mcp-agent-mail-db integration tests.
//!
//! Provides a spin-loop `block_on` that correctly drives futures to completion
//! without relying on the asupersync runtime's `thread::park()` mechanism.
//!
//! ## Why not use the runtime?
//!
//! All `SQLite` operations in this crate are synchronous (wrapped in
//! immediately-ready futures).  The asupersync runtime's `block_on` uses
//! `thread::park()` on `Poll::Pending`, which requires a proper waker to
//! `unpark` the thread.  Since no I/O driver or timer is registered with
//! `Cx::for_testing()`, a `Pending` return from internal bookkeeping (pool
//! acquire, `OnceCell` init) would park the thread forever.
//!
//! The spin-loop executor avoids this by repeatedly polling without parking,
//! which is safe because the futures resolve in very few polls (typically 0-1).

use asupersync::{Budget, Cx};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// Drive a future to completion using a spin loop.
///
/// Panics if the future does not resolve within `HANG_TIMEOUT`, which indicates
/// a genuine bug (not a waker/park issue).
fn spin_block_on_future<F: Future>(future: F) -> F::Output {
    const HANG_TIMEOUT: Duration = Duration::from_mins(1);
    const YIELD_EVERY: u64 = 1_000;
    const SLEEP_EVERY: u64 = 50_000;

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut future = Box::pin(future);
    let started = Instant::now();
    let mut polls = 0_u64;

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                polls = polls.saturating_add(1);
                if polls.is_multiple_of(YIELD_EVERY) {
                    std::thread::yield_now();
                }
                if polls.is_multiple_of(SLEEP_EVERY) {
                    let elapsed = started.elapsed();
                    assert!(
                        elapsed < HANG_TIMEOUT,
                        "spin_block_on: future did not resolve after {polls} polls over \
                             {elapsed:?} — likely a genuine hang (not a waker issue)"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

/// Run an async function with a `Cx::for_testing()` context.
///
/// Replacement for the runtime-based `block_on` that was causing hangs
/// (see br-2em1l).
#[allow(dead_code)]
pub fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: Future<Output = T>,
{
    let cx = Cx::for_testing();
    spin_block_on_future(f(cx))
}

/// Run an async function with a budget-constrained `Cx`.
#[allow(dead_code)]
pub fn block_on_with_budget<F, Fut, T>(budget: Budget, f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: Future<Output = T>,
{
    let cx = Cx::for_testing_with_budget(budget);
    spin_block_on_future(f(cx))
}

/// Run an async function with a request-scoped budget-constrained `Cx`.
#[allow(dead_code)]
pub fn block_on_request_with_budget<F, Fut, T>(budget: Budget, f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: Future<Output = T>,
{
    let cx = Cx::for_request_with_budget(budget);
    spin_block_on_future(f(cx))
}

/// Drive a pre-built future to completion using a spin loop.
///
/// Use this when you need to create the `Cx` yourself (e.g. in retry loops).
#[allow(dead_code)]
pub fn spin_poll<F: Future>(future: F) -> F::Output {
    spin_block_on_future(future)
}
