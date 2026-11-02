# GH362 canonical-ID candidate: local qualification packet

**This is a reviewable candidate and a reproducible qualification procedure. It does not activate the runtime change.** Apply the patch in a detached checkout and complete native qualification before adopting it. The packet does not add or enable a GitHub workflow.

The candidate addresses the unfinished canonical-ID walk from [issue #362](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/362). Its design, correctness boundaries, resource costs, previous unsuccessful experiments, and measurement limitations are explained in [design-and-performance.md](design-and-performance.md). The runtime changes and native regressions are in [final-runtime.patch](final-runtime.patch).

## Frozen inputs

- Baseline: `bc5d0a02a5d23a39fbb2fce83bb352a4bcc3df97`. The incumbent storage blob is `19de0eb88deef2ec9308554689cd69d707d94d4c`, identical to the storage code inspected at the starting main revision `0358e2d99da21661fcd679118e8000815ab865ab`.
- Candidate patch SHA256: `23ca383ac3b284600e239146fd0447564c339127ea76963380201d3ef28123d3`. It changes only storage `Cargo.toml`, `lib.rs`, `canonical_watch.rs`, and `canonical_id_tests.rs`.
- [baseline-fixture.patch](baseline-fixture.patch) adds test instrumentation and the shared public-writer fixture to the actual incumbent. Its production algorithm is unchanged. Its dev-only Nix feature alignment makes the compiled dependency features comparable. [baseline-fixture.json](baseline-fixture.json) records the fixture identity and deliberate differences.
- [frozen-inputs.json](frozen-inputs.json) identifies the packet files, helper scripts, and expected patched source hashes. The local orchestrator checks these before invoking Cargo.

The four evidence runners are unchanged from the checked versions. Portable copies of the existing build driver, compiler wrapper, and receipt emitter take explicit run/cache/toolchain paths. They keep the same compiler flags, reject overwritten results, pin `RUSTC`, clear conflicting Rust-flag/profile overrides, and check that the wrapper itself remains unchanged during compilation.

## Available evidence and outstanding gates

| Evidence | Status and scope |
| --- | --- |
| [Final storage production check](verification/storage-final-production-check.json), [raw log](verification/storage-final-production-check.log) | Passed on the final candidate storage source: 10.53 seconds, 587,128 KiB maximum child RSS, no warnings. This checks production code and excludes `cfg(test)` code. |
| [Earlier native libtest build](verification/candidate-lowmem-libtests-build.json), [raw log](verification/candidate-lowmem-libtests-build.log), [admission record](verification/candidate-lowmem-admission.json) | Earlier source snapshot (`lib.rs` SHA256 `8cc91355…`, test module `279c657a…`): exited 101 while the unchanged Asupersync dependency was killed before storage/test compilation. Maximum child RSS was 7,113,024 KiB; shared cgroup OOM-kill count rose from 8 to 10. This neither validates nor demonstrates a source failure in the final `633c631e…` candidate. |
| [First hinted-build admission outcome](verification/candidate-hint-admission-not-granted.json), [final admission outcome](verification/candidate-hint-final-admission-not-granted.json) | Both waits ended without launching Cargo. The final wait lasted 600.30 seconds and exited 75; its last sample reported two foreign Cargo processes and approximately 5.24 GiB usable headroom. The proposed compiler hint was never exercised here. No native binaries, tests, or paired measurements resulted. |
| [Earlier restored production-check log](verification/storage-restored-check.log) | Historical production-only check on an earlier source snapshot, approximately 1,492 seconds. The final source is qualified only by the separate final production-check record above. |
| [Initial qualification checkpoint](verification/initial-qualification.json) | Historical: seven real adapter-only tests, strict adapter Clippy, and a 101-message packed-Git fixture check. The watcher hash matches the final adapter; the older library/test hashes and its overall build-blocked status are superseded. It does not validate the final storage integration. |
| [Final Python runner controls](verification/runner-controls-final/controls.json) | Seventeen controls passed. They cover record schemas, process ownership and cleanup, nested PID namespaces, receipt binding, exact test counts, ordering, and failure retention. They are Python controls, not native Rust correctness evidence. |
| [Initial failed runner control](verification/runner-controls-initial-failure/controls.json) | Retained failure that revealed the nested PID namespace mismatch; the final controls verify the correction. |
| [Patch reproduction](verification/packet-preparation/qualification.json) | Passed using the explicit `--prepare-only` procedure: both patches apply to fresh detached baseline worktrees, all expected patched source hashes match, and both whitespace checks pass. No Cargo command was invoked. |
| Final storage/core libtest build, focused native regressions, incumbent negative control, and paired 5k/50k/150k measurements | Pending. The shared 8 GiB environment repeatedly encountered memory pressure in the unchanged Asupersync dependency and has not produced these native results. |

Neither the historical reporter timings nor the full-verifier control are current-main benchmark results. This packet contains no measured candidate-versus-incumbent speedup.

The initial qualification checkpoint refers to historical patch SHA256 `bf5400573cc4d303cfda4e5ac9f516bf936c8f27a486b6b598b5161ed226054b` (Git blob `05d964d93e9a0fca6337133fb8385a90a692a9ce`). That earlier patch is not an input to this reproduction; `final-runtime.patch` is authoritative. The preparation evidence retains the exact packet manifest checked during that run. Subsequent evidence/documentation additions and receipt-emitter hardening do not change either runtime patch; native execution of the final helper set remains pending.

## Host and tool prerequisites

Use a dedicated Linux host with comfortable memory headroom; 16 GiB is a practical target, not a guarantee or a claim that 8 GiB is intrinsically impossible. Allow sufficient disk space for Cargo caches, two checkouts, retained binaries, logs, and 450,000 tracked message copies at the largest fixture. Git, Python 3.11 or later, a C/C++ toolchain, make, Perl, and pkg-config are required. Vendored native dependencies include OpenSSL, zstd, and libgit2.

Install the repository-pinned `nightly-2026-08-31` toolchain. The expected rustc commit is `90850177249efe0321573c569aec5d12b257f8d6`. Supply its actual `bin` directory. The default procedure uses a fresh Cargo cache; an existing cache can be supplied explicitly after checking its configuration. Cargo uses `--locked` and the repository's pinned public Git dependencies; no hidden sibling checkout is needed.

Use a supported local filesystem for the watch-backed measurements. Record `/proc/sys/fs/inotify/max_user_watches` and `max_user_instances`; the runner records these without changing them. At 150,000 messages, the parent and independent foreign writer can briefly need two sets of inode watches, in addition to other watches owned by the same user. A low limit can therefore select the conservative full-verification path. Keep that result and report `watch_supported_and_enrolled`; do not label a fallback measurement as the bounded watch path. The procedure never raises global limits automatically.

Repository policy says builds and release gates run locally through RCH/DSR; see [reservation-restrictions.yml](../../../../.github/workflows/reservation-restrictions.yml) and [AGENTS.md](../../../../AGENTS.md). Arrange an approved local or RCH/DSR execution host for this procedure. RCH availability and forwarding are site-specific: do not assume that invoking the wrapper locally provisions a remote host. No Actions run or policy exception is requested by executing a read-only packet review.

## Review the patch without compiling

From a checkout containing this packet, run:

```bash
python3 scripts/perf/gh362_qualify.py \
  --repository /absolute/path/to/mcp_agent_mail_rust \
  --work-root /absolute/path/to/new-gh362-preview \
  --prepare-only
```

The repository must already contain the frozen baseline commit. This command verifies the packet hashes, creates two detached worktrees, applies each exact patch, compares the resulting source hashes, and runs `git diff --check`. It invokes no Cargo command. Worktrees and records are retained. Use a different new directory for a subsequent native run; the orchestrator refuses to reuse an existing run root.

## Run native qualification

Once the host is available, run explicitly:

```bash
python3 scripts/perf/gh362_qualify.py \
  --repository /absolute/path/to/mcp_agent_mail_rust \
  --work-root /absolute/path/to/new-gh362-native-run \
  --toolchain-bin /absolute/path/to/nightly-2026-08-31/bin
```

Optional `--cargo-home /absolute/path/to/cache` reuses a configured cache. `--build-timeout-seconds` defaults to 7200 for each build. The procedure proceeds serially:

1. Verify and apply both frozen patches in detached worktrees.
2. Check compiler identity, recheck candidate production code, compile candidate storage/core libtests, and copy the resulting binaries before reusing the target directory.
3. Emit the candidate build receipt and run the focused native regressions using the actual binary inventory. Only the explicitly named benchmark tests may remain ignored. Run the exact historical-rewrite counterpart separately and require it to pass.
4. Compile the incumbent with the same compiler, feature set, flags, wrapper, and profile. Emit a paired receipt that rejects differing configurations, changed source, or changed binaries.
5. Run the incumbent historical-rewrite test and retain its expected nonzero failure with `old in-place rewrite was missed`. An unrelated error, timeout, unexpectedly passing control, or missing test stops qualification. The expected failure remains a negative control, never a passing correctness test.
6. Run a 5k incumbent A/A check, then baseline/candidate/candidate/baseline at 5k, 50k, and 150k. Each arm records eight samples for `warm_single_writer`, `foreign_append`, and `controlled_fence_deferral`. Verify exact sample sets and equal seeded Git tree identities.

The development test profile is `opt-level=0`, debug info off, incremental compilation off, one Cargo job, and `RUSTFLAGS=-Zthreads=1`. Both arms use the same wrapper: one backend/linker job, with `-Zfewer-names=yes -Zmir-opt-level=0 -Zhint-mostly-unused=yes -Ztime-passes` scoped to Asupersync. These are compiler-memory tradeoffs, not release-profile results. The wrapper and exact commands are captured in the receipts.

## Broader repository gates remain separate

Successful execution of this packet establishes focused storage/core qualification and retains mechanism measurements. It does **not** fully qualify runtime activation or a release. The orchestrator intentionally does not run the broader repository gates required by `AGENTS.md`; those remain outstanding and need their own retained results on the candidate:

```bash
cargo check --workspace --all-targets
cargo check --workspace --lib --bins
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Run these through the approved local/RCH/DSR workflow with the repository-pinned compiler. The comprehensive plan's whole-server operational/performance acceptance also remains separate from this storage-mechanism experiment. Do not infer a pass for any unexecuted gate from the production-only check, adapter controls, or focused native results.

## Retained results and interpretation

The new run directory contains `qualification.json`, packet and host snapshots, detached worktrees, command logs, per-build manifests, copied binaries, normalized package-feature inventories, focused test inventories and per-group receipts, separate candidate/negative-control logs, and paired raw rows plus telemetry. A failed or interrupted stage records its failure and stops subsequent stages. Existing attempts are never overwritten or deleted automatically.

The measurements report publication latency, instrumented canonical metadata/enumeration operations, publisher-thread global-fence and project-lock hold times, and asynchronous commit-flush time. They do not count all filesystem syscalls or the foreign writer's resources. The cold publication starts with a cold canonical-verifier cache after filesystem/Git fixture seeding; it is not a cold-disk or full-server startup experiment. The fixture uses one seed Git commit, and controlled fence deferral intentionally constrains scheduling. Eight mechanism samples, an A/A check, and both pair orders do not close whole-server p99 or the comprehensive plan's broader operational gates.

Review the retained failures, watch capability, all three scenarios, both orders, and host conditions before assigning any performance conclusion. If native compilation remains unavailable, retain this packet and request an adequate approved local/RCH runner; any alternative CI use requires an explicit repository-policy exception. The runtime patch remains a candidate until those gates complete.
