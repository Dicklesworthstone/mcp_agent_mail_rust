# Issue 383: isolated index-observation evidence

Date: 2026-11-10  
Issue: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/383

## Scope and receipt status

This records a completed comparison of the unchanged baseline and proposed
compact-cache `decoded.rs` modules using real libgit2 and tempfile operations.
It measures an index-observation primitive, not the complete archive-maintenance
worker, retention correctness, or the reporter's deployed musl binary.

The values below were transcribed from completed command output retained in the
investigation conversation before the scratch environment became unavailable.
The original scratch logs, harness files, and fixtures are currently
inaccessible. This note does not attach, recreate, or claim to preserve those
original raw logs. Complete production-path validation is a separate gate.

## Source and build identity

Module path:
`crates/mcp-agent-mail-storage/src/recovery/message_reconcile/identity/staged/decoded.rs`

| Source | Identity |
| --- | --- |
| Baseline main commit | `ce1fac7f40fabd48732e80b9bc540c4e0accce3d` |
| Baseline Git blob | `2ac7d7b0bf9070a6c9faab1cedd7a3cfde64fe64` |
| Baseline SHA-256 | `1d0a1c91dd9cc4bf6b2791f0869a2801ec31002c26eb7410fcfea85682344d9e` |
| Final candidate Git blob preserved after the run | `79bd60edb2b3ba4aa2bd2d3c50032920795bd327` |
| Final candidate SHA-256 | `99c2c876e95cccdf1ecfbc1ce177f4b4ff129f329637b8d0314499fba8331dd1` |

The final repository file and the candidate source copy had the same SHA-256
before the final test/build/measurement sequence.

Compiler: `rustc 1.100.0-nightly (908501772 2026-08-30)`, installed as
nightly-2026-08-31; LLVM 23.1.0; host `x86_64-unknown-linux-gnu`.
The standalone Cargo dev/test profile used optimization level 1 and debug
information 0, with one Cargo build job. Dependencies were `git2 = 0.21.0`
with `vendored-libgit2` and `vendored-openssl`, and `tempfile = 3.27.0`.
The native libgit2 dependency was `libgit2-sys 0.18.8+1.9.7`.

Both source modules were included verbatim in one diagnostic crate. The only
outer compile wrappers were:

- `Result<T> = std::result::Result<T, Box<dyn std::error::Error>>`;
- `invalid(message)`, returning a boxed `std::io::Error::other(message)`;
- a default `CanonicalScanBudget` with a lifetime marker and a
  `checkpoint(&self, &'static str) -> Result<()>` method that returns `Ok(())`.

Those wrappers preserve the native index/cache operation but do not exercise
production cancellation, byte-budget charging, no-follow input reads, caller
recovery admission, or the maintenance scheduler. The harness performed an
ordinary fresh read of a controlled regular fixture file on every observation.

## Protocol and reproduction details

The driver created two synthetic repositories with native Git v2 indexes:
4,096 entries / 852,000 encoded bytes and 154,845 entries / 32,207,792 encoded
bytes. Each entry had a sorted 140-byte path:
`projects/gh383/messages/2026/11/{i:06}_`, followed by sufficient `x` bytes
and `.md` to reach 140 bytes. All entries referenced one real blob containing
`synthetic, non-deployment index fixture\n`, used mode `0o100644`, flags
140, extended flags zero, and zero stat/time fields. Each native v2 record
occupied 208 bytes.

In one process invocation, the driver ran the small fixture and then the large
fixture. For each it ran one cold baseline observation and one cold candidate
observation, followed by two rounds of 30 baseline and 30 candidate
observations. Each included module had its own static cache. Every observation
freshly read the entire index, called that module's `open`, checked entry
count, looked up `projects/gh383/messages/`, and consumed the returned
path/OID/mode/flags. Cache hits did not omit the fresh input read.

Wall time used `Instant`. Current-thread CPU used the sum of user and system
ticks from `/proc/thread-self/stat`, with verified `CLK_TCK=100`.
Per-phase I/O counters came from `/proc/self/io`. Zero CPU measurements are
below the 10 ms tick resolution, not proof of zero work.

The original executed commands, after setting the pinned toolchain,
`CARGO_HOME`, `CARGO_TARGET_DIR`, and `CARGO_BUILD_JOBS=1`, were:

```sh
cargo test --offline --manifest-path /workspace/scratch/c823833e0a4a/validation/index-probe/Cargo.toml
cargo build --offline --manifest-path /workspace/scratch/c823833e0a4a/validation/index-probe/Cargo.toml
/workspace/scratch/c823833e0a4a/validation/index-probe-target/debug/agent-mail-index-probe seed /workspace/scratch/c823833e0a4a/validation/index-probe/fixtures
/workspace/scratch/c823833e0a4a/validation/index-probe-target/debug/agent-mail-index-probe probe /workspace/scratch/c823833e0a4a/validation/index-probe/fixtures 30 100
```

These paths describe the former environment. Reproduction requires rebuilding
the small two-module driver with the wrappers and protocol above; the original
driver is not attached to this note.

## Completed native-module tests

The final suite completed with **23 passed, 0 failed, 0 ignored**, in **1.67
seconds**: 10 baseline tests and 13 candidate tests.

Candidate coverage included the 154,845-entry native fixture under the unchanged
32 MiB cache accounting limit, fresh-byte cache hits with a single native
decode, same-length changed input forcing another decode, preservation of old
immutable observations, native v2/v3/v4 formats, merge stages, raw non-UTF-8 and
long names, modes, extended flags, corrupt input, unsupported layouts,
capacity fallback, contention, and declared-count mismatch.

The final long-name fixtures were 4,096 and 4,097 bytes, both exceeding the
0x0fff index name-length sentinel. An earlier 8,192-byte fixture failed during
native serialization before candidate code: the path plus header exceeded
libgit2's 8 KiB entry-write buffer. The corrected fixture retained the long-name
and field-equivalence assertions; no production admission check was relaxed.

The assigned file also passed rustfmt and `git diff --check`. The isolated
Clippy review resolved new implementation diagnostics, but was not a clean
workspace Clippy gate: remaining diagnostic-crate warnings concerned wrappers,
the unchanged baseline, and existing visibility items included at different
module nesting depth.

## Final-source measurements

| Phase | Implementation | Observations | Wall seconds | Thread CPU seconds | Logical rchar | Logical wchar |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Small, warm round 1 | Baseline | 30 | 0.001953 | 0.00 | 25,560,112 | 0 |
| Small, warm round 1 | Candidate | 30 | 0.001765 | 0.00 | 25,560,114 | 0 |
| Small, warm round 2 | Baseline | 30 | 0.002845 | 0.01 | 25,560,114 | 0 |
| Small, warm round 2 | Candidate | 30 | 0.002559 | 0.00 | 25,560,114 | 0 |
| Large, cold | Baseline | 1 | 0.248611 | 0.25 | 64,415,719 | 32,207,792 |
| Large, cold | Candidate | 1 | 0.264110 | 0.26 | 64,415,722 | 32,207,792 |
| Large, warm round 1 | Baseline | 30 | 6.209994 | 6.20 | 1,932,468,238 | 966,233,760 |
| Large, warm round 1 | Candidate | 30 | 0.212634 | 0.21 | 966,233,881 | 0 |
| Large, warm round 2 | Baseline | 30 | 6.121896 | 6.12 | 1,932,468,241 | 966,233,760 |
| Large, warm round 2 | Candidate | 30 | 0.183483 | 0.19 | 966,233,881 | 0 |

Across **60 large warm observations**, baseline thread CPU was **12.32 seconds**
and candidate thread CPU was **0.40 seconds**, approximately 30.8 times less CPU
for this isolated operation. Each candidate observation still read the full
32,207,792-byte current input. The repeated private snapshot writes and native
decodes were avoided.

`rchar` and `wchar` are logical I/O counters, not physical disk traffic.
All warm phases reported zero `read_bytes` and `write_bytes` in
`/proc/self/io`. Baseline cold observations reported 856,064 small-fixture and
32,210,944 large-fixture `read_bytes`; no phase reported `write_bytes`.
The candidate cold input was already filesystem-cached, so the cold timing
is not a filesystem-cache-neutral comparison.

## Practical interpretation and remaining validation

These results establish the repeated native-decode cost and the compact-cache
effect for unchanged large-index observations. They do not establish the full
worker's duty cycle, repair convergence, backup deletion behavior, or the
reporter's exact CPU use. Compiler/profile, filesystem caching, shared-host
load, and tick resolution limit performance generalization.

The cache retains native validation, exact fresh-byte matching, and the existing
32 MiB retained-entry accounting bound. It caches index interpretation, not a
successful repair/admission verdict. Native decoding still occurs for changed
input, and the original fallback remains for unproved layouts or capacity.
Fresh complete reads and comparisons must remain charged to production
maintenance budgets; this optimization is not permission to accelerate
unbounded scans.

The populated production-worker CPU, sustained-backlog, cancellation,
recovery-convergence, retention-safety, and workspace validation results must be
recorded separately. No deployment backups were deleted and no release
artifacts were published by this investigation.
