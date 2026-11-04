# sqlmodel-pool

Structured-concurrency-aware connection pooling.

## Role in the SQLModel Rust System
- Budget-aware acquisition via Cx timeouts and cancellation.
- Health checks and lifecycle management for connections.
- Works with any sqlmodel-core::Connection implementation.

## Usage
Most users should depend on `sqlmodel` and import from `sqlmodel::prelude::*`.
Use this crate directly if you are extending internals or building tooling around the core APIs.

## Links
- Repository: https://github.com/Dicklesworthstone/sqlmodel_rust
- Documentation: https://docs.rs/sqlmodel-pool

## Agent Mail #374 backport

Agent Mail vendors the published `sqlmodel-pool 0.5.0` crate to repair pending
request accounting when an HTTP deadline drops a waiting acquisition future.
The package version and dependency requirements remain those of the published
crate. This copy includes all published Rust source and tests, the normalized
and original manifests, the packaged lockfile, and `.cargo_vcs_info.json`.

### Provenance

| Item | Exact source |
| --- | --- |
| Registry archive | `https://static.crates.io/crates/sqlmodel-pool/sqlmodel-pool-0.5.0.crate` |
| Archive SHA-256 | `d090572646f14c79363c6a78fe073afdb2213b77421b1815b31a6761e1f3ffa2` |
| Published source commit | `12d490b22ce3a82533591b5b39d2318b21da1f0f` |
| Source path | `crates/sqlmodel-pool` in `Dicklesworthstone/sqlmodel_rust` |
| Upstream waiter guard | `ee08466f00162ea21c0f9cf05da1f05f04d44758` |
| License | `LICENSE`, copied unmodified from the published source commit; Git blob `3d414da6ff83defe084707be6030866e8a7fb794` |

The registry archive checksum matches the original Agent Mail `Cargo.lock`
entry. The upstream guard is part of a larger, unreleased pool acquisition
change; upgrading to published SQLModel `0.5.1` alone does not contain it.

### Local changes

Only `src/lib.rs` has production code changes:

- `AcquireAction::Wait` owns a `WaiterGuard` from the same point that increments
  `waiter_count` until that wait ends. Its destructor unregisters on wake,
  timeout, cancellation, unwind, or hard future drop.
- The two manual decrement sites are removed to avoid double accounting.
- The existing enum-variant test constructs and drops the owned wait variant,
  following the upstream test adjustment.
- Admission, connection creation, validation, retirement, transaction behavior,
  and the rest of the locked dependency graph retain the published behavior.

Three added deterministic tests cover repeated drops with a surviving waiter,
actual wakeup and capacity reuse independently of pending statistics, and pool
closure while a different caller still owns the only connection. The liveness
test is deliberately independent of the pending-count assertions so it can be
run as an unpatched control to distinguish telemetry leakage from lost capacity.
These are pool-level tests; Agent Mail's native HTTP cancellation regression
remains the integration gate.

The additional non-source file is the upstream `LICENSE`; this README records
provenance and the patch scope. The root `[patch.crates-io]` selects this copy.
It is excluded from Agent Mail workspace membership and can run its own tests:

```sh
cargo test --manifest-path vendor/sqlmodel-pool/Cargo.toml --locked --lib gh374_ -- --nocapture
```

Remove this override when Agent Mail deliberately adopts a published dependency
with equivalent cancellation coverage and its native regression passes against
that dependency. Do not substitute an unpinned sibling checkout.
