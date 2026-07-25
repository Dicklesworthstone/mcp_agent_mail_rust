//! Stack sizing for spawned worker threads.
//!
//! # Why this exists
//!
//! A stack overflow is not a recoverable error in Rust: the runtime prints
//! `fatal runtime error: stack overflow` and calls `abort()`. It cannot be
//! caught, there is no unwinding, and every other thread's in-flight work dies
//! with it. A single deep call chain on any one worker takes down the whole
//! daemon.
//!
//! Rust gives spawned threads a 2 MiB stack by default, while the main thread
//! inherits the process rlimit (typically 8 MiB). GH#202 hit exactly this: the
//! `am-archive-read` worker ran the full archive reconstruction/salvage path on
//! 2 MiB and aborted the process on a production-scale mailbox, while the same
//! code was fine when driven from the main thread.
//!
//! # Why every worker, rather than the ones that look risky
//!
//! The first fix for GH#202 sized only the threads that could be *proven* to
//! reach `reconstruct_from_archive*`. That proof missed a second thread (the
//! operator dashboard's refresh worker) which reached the same path by a longer
//! route. Auditing call graphs by hand does not survive contact with a codebase
//! this size, and any future edit can deepen a path that is shallow today.
//!
//! Sizing is close to free, so we do not ration it. Thread stacks are reserved
//! address space, committed lazily by the OS one page at a time; an untouched
//! 32 MiB stack costs ~0 resident memory. Even 30 workers reserve well under a
//! gigabyte of a 128 TiB address space. Trading that for "the daemon cannot
//! abort itself this way" is not a close call.
//!
//! # What this does not fix
//!
//! A generous stack bounds *deep* recursion, not *unbounded* recursion. If a
//! call chain recurses proportionally to archive or mailbox size, no fixed
//! stack is sufficient and the recursion itself has to be bounded or made
//! iterative. Sizing buys headroom; it is not a substitute for that.
//!
//! # The one cost that is not free
//!
//! Reserving more per thread makes `Builder::spawn` marginally more likely to
//! fail where address space is *limited* rather than merely large — notably
//! under a `ulimit -v` cap, where ~30 workers now reserve on the order of a
//! gigabyte that previously cost ~60 MiB. Default Linux and macOS
//! configurations do not cap virtual memory, so this is a deployment-specific
//! concern; if you run under `ulimit -v`, size the cap accordingly or lower
//! [`WORKER_STACK_ENV`]. Note that at least one spawn site treats failure as
//! fatal, so a spawn failure is not a soft degradation.

/// Default stack for spawned workers that may touch mailbox data.
///
/// Validated against a production-scale mailbox (~2.6k agents / ~18.3k
/// messages) in GH#202, where 32 MiB carried the reconstruction/salvage path
/// through a sustained soak with concurrent probes.
pub const WORKER_STACK_SIZE: usize = 32 * 1024 * 1024;

/// Floor for the operator override. Below this GH#202 reproduces.
pub const WORKER_STACK_SIZE_MIN: usize = 8 * 1024 * 1024;

/// Ceiling for the [`WORKER_STACK_ENV`] override, so a typo in that variable
/// cannot reserve terabytes.
///
/// This bounds our own knob only. `RUST_MIN_STACK` is deliberately *not*
/// clamped by it: that variable is a global std setting the operator has
/// already chosen for every thread in the process, and silently capping it
/// here would be surprising. See [`resolve_worker_stack_size`].
pub const WORKER_STACK_SIZE_MAX: usize = 512 * 1024 * 1024;

/// Environment override, in megabytes.
pub const WORKER_STACK_ENV: &str = "MCP_AGENT_MAIL_WORKER_STACK_MB";

/// Legacy name for the same knob, kept so existing GH#202 workarounds and
/// deployed configs keep working.
pub const LEGACY_WORKER_STACK_ENV: &str = "MCP_AGENT_MAIL_READ_SNAPSHOT_STACK_MB";

/// Stack size, in bytes, for a spawned worker thread.
///
/// Use this for **every** thread this project spawns that can touch mailbox
/// data, rather than trying to decide per-thread which ones go deep. See the
/// module docs for why that judgment call is not worth making.
///
/// Tunable via [`WORKER_STACK_ENV`] (or the legacy
/// [`LEGACY_WORKER_STACK_ENV`]), clamped to
/// `[WORKER_STACK_SIZE_MIN, WORKER_STACK_SIZE_MAX]`.
#[must_use]
pub fn worker_stack_size() -> usize {
    resolve_worker_stack_size(
        read_usize_env(WORKER_STACK_ENV).or_else(|| read_usize_env(LEGACY_WORKER_STACK_ENV)),
        read_usize_env("RUST_MIN_STACK"),
    )
}

fn read_usize_env(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
}

/// Pure resolution for [`worker_stack_size`], split out so the clamping and
/// `RUST_MIN_STACK` precedence rules are testable without mutating process
/// environment (several crates here are `#![forbid(unsafe_code)]`, and
/// `set_var` is `unsafe` in edition 2024).
///
/// `Thread::stack_size` takes precedence over `RUST_MIN_STACK` in std, so
/// `RUST_MIN_STACK` is folded in explicitly: an operator who already raised it
/// as a GH#202 workaround keeps that headroom instead of being silently
/// lowered to our default.
#[must_use]
pub fn resolve_worker_stack_size(
    configured_mb: Option<usize>,
    rust_min_stack_bytes: Option<usize>,
) -> usize {
    let configured = configured_mb
        .and_then(|mb| mb.checked_mul(1024 * 1024))
        .unwrap_or(WORKER_STACK_SIZE)
        .clamp(WORKER_STACK_SIZE_MIN, WORKER_STACK_SIZE_MAX);

    configured.max(rust_min_stack_bytes.unwrap_or(0))
}

/// A [`std::thread::Builder`] named `name` and pre-sized via
/// [`worker_stack_size`].
///
/// Convenience for *new* spawn sites, so they cannot silently inherit the
/// 2 MiB default. Existing sites call `.stack_size(worker_stack_size())`
/// explicitly on their own builders — both routes resolve to the same policy,
/// so either is correct; this one is simply harder to forget.
#[must_use]
pub fn worker_thread(name: impl Into<String>) -> std::thread::Builder {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(worker_stack_size())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_clears_the_2_mib_spawned_thread_default() {
        assert_eq!(resolve_worker_stack_size(None, None), WORKER_STACK_SIZE);
        assert!(
            WORKER_STACK_SIZE > 2 * 1024 * 1024,
            "default must exceed the 2 MiB spawned-thread default that GH#202 overflowed"
        );
    }

    #[test]
    fn override_is_clamped_rather_than_honored_blindly() {
        // Below the floor clamps up, so GH#202 cannot be reintroduced by a
        // stray small value.
        assert_eq!(resolve_worker_stack_size(Some(1), None), WORKER_STACK_SIZE_MIN);
        // Absurd values clamp down rather than reserving terabytes.
        assert_eq!(
            resolve_worker_stack_size(Some(999_999), None),
            WORKER_STACK_SIZE_MAX
        );
        // An overflowing multiply falls back to the default instead of wrapping.
        assert_eq!(
            resolve_worker_stack_size(Some(usize::MAX), None),
            WORKER_STACK_SIZE
        );
        // A sane override is honored verbatim.
        assert_eq!(resolve_worker_stack_size(Some(64), None), 64 * 1024 * 1024);
    }

    #[test]
    fn rust_min_stack_headroom_is_never_silently_lowered() {
        assert_eq!(
            resolve_worker_stack_size(None, Some(128 * 1024 * 1024)),
            128 * 1024 * 1024
        );
        // ...but a smaller RUST_MIN_STACK never drags the default down.
        assert_eq!(
            resolve_worker_stack_size(None, Some(1024 * 1024)),
            WORKER_STACK_SIZE
        );
    }

    #[test]
    fn worker_thread_builder_is_sized() {
        // Spawning through the helper must survive a frame depth that would
        // abort on the 2 MiB default, i.e. the size is really applied.
        fn burn(depth: usize) -> usize {
            let mut frame = [0u8; 4096];
            frame[0] = depth as u8;
            frame[4095] = depth as u8;
            std::hint::black_box(&frame);
            if depth == 0 {
                return 0;
            }
            burn(depth - 1) + usize::from(frame[0])
        }

        // 1024 frames x ~4 KiB is ~4 MiB: over the 2 MiB default, under the floor.
        let handle = worker_thread("worker-stack-probe")
            .spawn(|| burn(1024))
            .expect("spawn worker stack probe");
        handle.join().expect("worker stack probe must not overflow");
    }
}
