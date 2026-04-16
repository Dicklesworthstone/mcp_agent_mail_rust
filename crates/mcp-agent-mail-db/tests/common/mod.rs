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

/// Drive a future to completion using a spin loop.
///
/// Panics if the future does not resolve within `MAX_POLLS` iterations,
/// which indicates a genuine bug (not a waker/park issue).
fn spin_block_on_future<F: Future>(future: F) -> F::Output {
    const MAX_POLLS: u64 = 500_000;

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut future = Box::pin(future);

    for poll_count in 0..MAX_POLLS {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                if poll_count % 10_000 == 9_999 {
                    std::thread::yield_now();
                }
            }
        }
    }
    panic!(
        "spin_block_on: future did not resolve after {MAX_POLLS} polls — \
         likely a genuine hang (not a waker issue)"
    );
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
