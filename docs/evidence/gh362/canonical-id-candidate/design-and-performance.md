# GH362 candidate: canonical-ID verification with synchronous change evidence

This report accompanies a reviewable candidate patch. The final production
library passed a native check, but the integrated tests and mailbox benchmarks
have not run. The patch is not a claim of a validated runtime improvement.

The candidate makes a complete filesystem change witness, rather than the age of
an in-process cache, the authority for skipping unchanged canonical messages.
On supported local Linux filesystems, routine publication verifies the changed
files and the outgoing batch without enumerating all historical messages.
Missing or invalid notification authority uses the conservative full inventory.

The complete implementation and test sources are retained in
[`final-runtime.patch`](final-runtime.patch). Within that patch, implementation
lives in `crates/mcp-agent-mail-storage/src/canonical_watch.rs` and the
canonical-ID/publication functions in `crates/mcp-agent-mail-storage/src/lib.rs`.
The new `crates/mcp-agent-mail-storage/src/canonical_id_tests.rs` contains real
publication regressions and the scale harness. Commands below apply after
applying the retained patch to its recorded baseline.

## Evidence and the actual starting point

[GH362](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/362)
reported three historical archive-size costs. The reporter measured main
`b3c26ef0`, and said the inspected files were unchanged at `fee33a98`:

| Canonical messages | Historical canonical-ID walk |
| ---: | ---: |
| 5,000 | 28 ms |
| 50,000 | 395 ms |
| 150,000 | 1,132 ms |

These are the reporter's historical measurements, not results from this change
or measurements of present `main`. The
[maintainer response](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/362#issuecomment-6287923343)
documents the empty in-memory Git index optimization (`062275fe`), generation
artifact lookup optimization (`b87f948c`), and the then-unfinished canonical walk.
The
[October 31 follow-up](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/362#issuecomment-6544119202)
measured reservation reconciliation in **released v0.3.39**. It does not measure
the canonical verifier or the newer source optimizations.

The starting checkout for this work was
[`0358e2d99da21661fcd679118e8000815ab865ab`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0358e2d99da21661fcd679118e8000815ab865ab).
It already contained additional work from `br-kp1in.47`: an O(batch)
single-writer reverse index, a four-file recent window, a five-minute full
revalidation interval, and preparation outside the global publication fence
when possible. It also reused a deferred proof when process-local project lock
generation and the `messages/` root stamp appeared unchanged. Therefore this
patch must not be presented as introducing the first bounded routine-send path
to that baseline.

After the execution environment was restarted, the checkout advanced to
[`bc5d0a02a5d23a39fbb2fce83bb352a4bcc3df97`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bc5d0a02a5d23a39fbb2fce83bb352a4bcc3df97).
The intervening changes did not change the storage implementation. The actual
pre-change revision comparison uses `bc5d0a`, with only test fixtures and timing
counters added to its Rust source. Its development-only nix features are aligned
with the candidate so both test binaries use the same dependency feature graph;
the baseline's production verifier remains unchanged.

The relevant planning context is the archive/locking work in
[`PLAN_TO_PORT_MCP_AGENT_MAIL_TO_RUST.md`](../../../../PLAN_TO_PORT_MCP_AGENT_MAIL_TO_RUST.md),
the write/recovery contract in [`SYNC_STRATEGY.md`](../../../../SYNC_STRATEGY.md), and
beads `br-dgut7`, `br-kp1in.47`, and `br-0i0g4` in
[`.beads/issues.jsonl`](../../../../.beads/issues.jsonl). The bead history records a
previous unmerged inotify experiment and unsuccessful whole-server performance
comparisons. Those experiments are not validation of this implementation.

Neither the Git index optimization nor reservation artifact lookup is
reimplemented here.

## Safety gap in the starting cache

Two completed changes could escape its routine checks:

1. A historical canonical file outside the recent window could be rewritten in
   place to claim the outgoing ID. Its parent directory need not change, and
   the batch need not touch that historical month.
2. A different process could acquire the project archive lock and modify or
   append a canonical file in an existing month while a prepared publication
   waited for the global fence. Its lock acquisition does not advance this
   process's lock-generation map, and the `messages/` root need not change.

The second case obeys the project lock protocol. It is not covered by assuming
that foreign writers are always uncooperative. A recent timestamp, an unchanged
month directory, or an unchanged process-local generation cannot prove that an
ID is absent from all historical canonical files.

## Authority and publication protocol

`CanonicalIdCache` retains the file-to-ID parse cache and reverse `by_id` index.
It additionally owns a `CanonicalWatch`, a set of dirty canonical paths, and a
revision. Only a complete watch that has been drained synchronously can
authorize the incremental path. The existing five-minute metadata audit remains
as defense in depth: routine reuse requires both a healthy notification witness
and an audit newer than that interval. Cache age alone is never authority.

| Phase | Work and required evidence |
| --- | --- |
| Cold initialization | Enroll each canonical directory before enumerating it and each canonical inode before inspecting it. Build the complete index, then drain notifications and refresh changes that occurred during the scan. A partial inventory never authorizes the fast path. |
| Routine publication | Drain to `EAGAIN`, re-read dirty canonical files, maintain both directions of the index, and check outgoing destinations and candidate colliding paths. No unchanged historical directory enumeration is needed. |
| Own publication | Record each successfully published canonical ID/path and enroll its inode. Keep notification events available for subsequent verification; do not assume every queued event came from this process. |
| Foreign append or rewrite | Refresh the named files, even when their byte length and mtime were restored. Changes in a known historical month do not require another complete inventory. |
| Deferred proof | Require the same cache revision and complete event stream, in addition to project-lock/root checks, before reusing prepared absence evidence. A changed proof is prepared again outside the global fence. |
| Before immutable installation | Drain again after preparation and immediately before the first no-clobber install, after staging and file synchronization. Detected drift causes retry before publishing new copies. |
| Repeated contention | After four optimistic attempts, acquire the publication fence and project locks, then prepare fresh evidence once under those guards. This guarantees another progress opportunity without reusing stale prepared evidence or removing any pre-install conflict checks. |
| Periodic audit | After five minutes, enumerate canonical history and revalidate its metadata, rebuild the complete witness, then resume the bounded path. This does not force byte reads of every unchanged cached file. |
| Invalidation | Discard notification authority on queue overflow, watch loss, namespace replacement, ambiguous descriptors, failed enrollment, or verification failure. Rebuild before trusting a cached absence. |

Directory watches cover new names, removal, and namespace changes. Inode
watches additionally cover writes through links outside a canonical directory;
watching only the directory would miss that route. There is one notification
descriptor per project cache, not one open descriptor per historical file.
This follows the Linux
[inotify design](https://github.com/torvalds/linux/blob/v6.12/Documentation/filesystems/inotify.rst)
and [inotify API semantics](https://man7.org/linux/man-pages/man7/inotify.7.html).

There is no background notification thread. The publishing thread drains its
private nonblocking event stream under the cache mutex. It treats notifications
as evidence of dirtiness, not an exact count of writes. Drain work is bounded
at 65,536 events/interruptions, incremental refresh makes at most three rounds,
and message publication makes at most four optimistic attempts before one
fresh synchronized preparation. A continuously changing archive can still
produce a retryable canonical-archive contention error when the final identity
checks cannot establish stable evidence.

### Synchronization scope

Read-only preparation holds the project locks while staying outside the global
publication fence when the caller does not already own that fence. The
project-lock-first attempt uses a nonblocking fence acquisition. On contention,
it releases the project locks before waiting for the fence, preserving the
established lock ordering.

After a wait, a valid notification/revision proof can be reused. If that proof
changed, the attempt releases its synchronization guards and retries
preparation outside the fence. After four such attempts, the bounded fallback
acquires the fence and project locks before preparing once more. A healthy
watch keeps this final preparation proportional to changed files; cold,
periodic, or unsupported verification may still do O(history) work while the
fence is held. This last preparation prevents otherwise legitimate cooperating
writers from exhausting the optimistic retry budget indefinitely. The fallback
retains all staging and pre-install identity checks. A caller
that already owns an outer mutation window also cannot have its fence released
by this helper. Thus the change removes history-sized work from routine sends
between audits where a complete witness is available. Cold, periodic, and
fallback verification still cost O(history); they run outside the global fence
when the locking conditions above permit. The change does not claim that every
scan runs outside every publication-critical section.

### Duplicate identity, immutable evidence, and recovery

A positive ID already present at another canonical path is refused. An
identical retry at its original path remains valid, while differing bytes at
any existing immutable copy are refused. The existing stage/sync/no-clobber
installation path remains responsible for retaining conflicting evidence.
Batch identity and destination conflicts are checked before publishing the
first bundle. A later I/O failure can leave a partial bundle; its already
installed canonical bytes remain available for an idempotent retry to finish
missing outbox/inbox copies.

No persistent cache file, alternate archive format, or new database authority
is introduced. A process restart rebuilds from canonical files. A failed or
interrupted publication invalidates cached authority; a partial canonical file
that was successfully installed is discovered by the next inventory. Existing
archive recovery and reconstruction continue to use the same canonical files.
Legacy ID-less publication retains destination-byte checks and invalidates an
older canonical-ID proof instead of carrying it across unchecked namespace
changes.

## Supported path and limits

The optimized witness is enabled on Linux for the allowlisted local filesystem
types: ext2/ext3/ext4, btrfs, tmpfs, f2fs, XFS, and overlayfs through its mounted
namespace. Enrollment rejects symlinks, wrong object types, and a nested device
different from the project root. Other operating systems, remote or unknown
filesystems, and failed inotify enrollment retain full verification.

Bootstrap, periodic metadata audits, and cache rebuild remain O(history), and
user/kernel watch state is O(canonical files + canonical directories). A
150,000-message archive needs
approximately 150,000 file watches in addition to its directory watches.
`max_user_watches`, `max_user_instances`, the kernel event queue limit, or memory
pressure can prevent complete enrollment. The code does not change system
limits or silently trust a partial watch. A missing witness means the warmed
reverse index is insufficient, so future publications use conservative scans.

Creating a new year/month namespace, replacing a watched directory, or finding
two canonical path spellings for one inode can invalidate the witness and
require a rebuild. An ordinary append to an already watched month is handled
incrementally. The adapter refuses to repurpose descriptors with ambiguous
queued events rather than guessing which inode an event described.

The event witness covers ordinary local filesystem writes and assumes the
mounted namespace remains stable. Linux does not report writable `mmap` stores
through inotify; remote filesystem changes and mounting over a watched
directory are also outside its event guarantees. Administrative mount changes
require reinitialization. These
[documented inotify limitations](https://man7.org/linux/man-pages/man7/inotify.7.html)
are not solved by this cache or its retained periodic metadata audit. The audit
does not promise immediate detection or re-read bytes when cached file metadata
still matches. Advisory project locks also cannot exclude an
uncooperative writer changing evidence after the final check. The old full walk
did not provide that exclusion either. Cooperating archive writers must use the
project lock; canonical evidence should not be edited through writable mappings
or a changed backing mount while the service is active.

## Correctness regressions

The production-path tests cover:

- A same-size old-file ID rewrite, with mtime restored and parent/root stamps
  unchanged, for single and batched publication.
- A duplicate inserted into an untouched historical month, a replaced month,
  and modification through an external hardlink.
- Changes after preparation and after staging/fsync, on a supported Linux
  notification path.
- A separate process taking the real project lock and rewriting old history
  during fence deferral, with unchanged parent process lock generations.
- A separate-process append in an existing historical month, with bounded
  verification work and subsequent collision detection.
- Warm reverse-index data with its watch removed, requiring a full inventory.
- An aged but otherwise healthy notification-backed cache, requiring a full
  periodic inventory before the next send returns to bounded verification.
- A real conservative no-watch path forced by canonical hardlink ambiguity,
  which must repeat its inventory after a foreign writer wins during deferral.
- Four deterministic fence deferrals with intervening cooperating project-lock
  writers, followed by successful fresh synchronized preparation. Separate
  cases cover lock-generation changes, distinct canonical additions, and a
  conflicting final addition that must still be refused without new copies.
- Cold process startup, explicit invalidation, partial installation and retry,
  and concurrent unique/conflicting sends.
- An actual worker process exiting with code 77 immediately after installing
  the canonical copy, before any remaining copy is installed and without
  running Rust destructors. A second fresh process inventories that evidence,
  completes the identical bundle while retaining its canonical inode/bytes,
  and refuses a conflicting identity. This tests process interruption, not
  power-loss durability.

The adapter additionally tests actual local inode/namespace changes, open
writers, hardlinks, symlink refusal, replacement, and watch loss. Queue overflow
and unmount handling use explicit notification fault injection; those injected
conditions are not described as real kernel queue-overflow/unmount experiments.

Run the focused real storage and adapter tests with the pinned toolchain:

```sh
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib canonical_id_tests:: -- --nocapture
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib canonical_watch:: -- --nocapture
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib canonical_id_ -- --nocapture
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib message_publication_ -- --nocapture
```

The notification-specific tests explicitly require supported local storage.
Prior-call rewrites, process restart, and conservative deferred-proof checks
remain platform-neutral. All functional tests use temporary storage roots;
subprocess cases assert that their exact selector executed one test.

## Scale benchmark protocol

Run explicitly; it creates large temporary archives and Git history:

```sh
cargo test --release --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib canonical_id_tests::canonical_publication_scale_benchmark -- --exact --ignored --nocapture
```

The harness seeds 5,000, 50,000, and 150,000 canonical messages across twelve
historical months, with 15,000, 150,000, and 450,000 tracked Git paths. Historical
outbox/inbox copies are hardlinks to their corresponding canonical file to
bound fixture disk use. Each canonical message has its own inode. Measured new
publications use the real separate-copy stage/sync/no-clobber writer. The fixture
contains text envelopes/bodies and one recipient per message; it does not
simulate attachments, database queries, MCP transport, or a multi-agent server
load. Its purpose is archive publication and canonical verification scaling.

Before sampling, setup streams the historical blobs into a Git pack with
`git fast-import`, retaining the initialized parent commit and its archive
attributes. `git read-tree HEAD` populates the real on-disk index without
changing the fixture files. This avoids a large temporary population of loose
Git objects; it does not change the three-copy tracked tree or the measured
publication path. The historical corpus is introduced in one seed commit;
the fixture does not reproduce a long sequence of historical Git commits.
Setup then ages files beyond the racy timestamp window,
records cold publication separately, and settles warm publication/commit work.
Here, "cold" means an empty application canonical-ID cache and no existing
notification witness. Fixture creation has already touched the filesystem and
its page cache; this is not a cold-disk or process-startup measurement.
It runs eight alternating pairs at each size. It asserts that queued requests
and active commit work are empty after each flush, since the production flush
API otherwise returns silently after its timeout. It also requires zero
cumulative coalescer commit-group/spill errors, including failures later
recovered by the coalescer's requeue-and-retry path. That counter does not count
every internal Git lock/ref retry. A recorded coalescer error fails the fixture
even if the queue eventually settles. These checks happen outside the reported
publication and flush timings.

| Mode | What runs |
| --- | --- |
| `production` | The public single-message publication API with the incremental verifier. |
| `full_verification_control` | The live production full identity collector, followed by the same durable append/no-clobber/coalescer-enqueue path. |

The full-verification control preserves the production parse cache, so it can
reuse unchanged parsed IDs while enumerating and checking every historical
file's metadata and rebuilding the witness. It does not force byte reads of
every canonical file.

The control is a mechanism comparison with conservative full verification in
the **same invocation**. It is not the original reported revision, v0.3.39, or
the starting `0358e2d` periodic cache. A claim of improvement over that actual
starting implementation requires running that implementation separately with
the same fixture, compiler/profile, host, and workload. No historical timings
should be combined with these samples into a speedup ratio.

### Actual pre-change revision comparison

The separate ignored `canonical_publication_public_writer_benchmark` worker
always calls the public writer without selecting a verification implementation.
Its fixture, foreign-sender worker, and sample code are shared unchanged between
the actual pre-change `bc5d0a` test build and the candidate. The old build adds
only test instrumentation and development-only nix feature alignment; its
canonical verifier stays unchanged. A separate
read-only enrollment observer reports `null` for the old build, which has no
notification witness, and the actual `Some(true/false)` state for the candidate.

For each archive size, run both retained executables in alternating AB/BA order
from one orchestrator invocation. Record source and binary hashes, all raw
outputs, exit statuses, compiler settings, and filesystem/kernel limits. This
worker has three separately reported scenarios:

| Scenario | Timed parent operation |
| --- | --- |
| `warm_single_writer` | A regular send from an already warmed process, with no intervening foreign publication. |
| `foreign_append` | A regular send after a separate process has completed a public message publication into the existing current month, flushed its Git work, and exited. |
| `controlled_fence_deferral` | A regular send deliberately deferred behind a held global fence. After preparation releases its project locks, the controller takes the real project lock, appends a distinct current-month canonical file and syncs its bytes, then releases the fence. |

The foreign sender uses the real public writer, including its project lock and
immutable-copy path. Its startup, cold verification, publication, and commit
flush are **outside** the parent sample. This measures how a warm publisher
reacts to completed foreign changes; it does not measure the combined two-send
transaction or whole-server throughput.

The controlled-deferral case forces a same-process lock-generation change and
real canonical evidence between preparation and publication. It can expose the
incumbent's history-sized verification under the global fence and distinguish
the candidate's retry outside that fence. Its wall time includes the controlled
append and scheduling wait; controller-thread lock holds are excluded from the
publisher's thread-local counters. This is a deterministic contention scenario,
not an estimate of real production p99. `AM_CANONICAL_BENCH_SCENARIO=all` seeds
one fixture per executable and records all three scenarios with distinct IDs.

One worker invocation, for example:

```sh
AM_CANONICAL_BENCH_MESSAGES=5000 AM_CANONICAL_BENCH_ROUNDS=8 AM_CANONICAL_BENCH_SCENARIO=all cargo test --release --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib canonical_id_tests::canonical_publication_public_writer_benchmark -- --exact --ignored --nocapture
```

This single command is one arm; the paired revision result requires both actual
executables in the alternating run. Repeat for 50,000 and 150,000 histories,
retaining unsupported-watch and error outcomes.

### Measurement scope

Each sample prints one JSON object:

| Field | Scope |
| --- | --- |
| `publication_us` | Wall time of actual storage publication through commit enqueue; excludes setup, database/MCP work, and waiting for the later Git commit to finish. |
| `canonical_read_dirs` | Calls through the canonical verifier's directory-enumeration wrapper. |
| `canonical_metadata_calls` | Calls through its metadata wrapper, including watcher enrollment. This is not a total process `statx` count; path-prefix/read helpers, Git, and other I/O have additional metadata work. |
| `publication_thread_fence_held_us` | Sum of global publication-fence hold time on the publishing thread. Excludes time waiting for that fence and holds by background commit workers. |
| `publication_thread_project_lock_held_us` | Sum of time from the project-lock acquisition method returning through release on the publishing thread, including read-only preparation. Excludes acquisition waits, lock-owner metadata work inside acquisition, and holds by background workers. The project and global intervals overlap, so these values must not be added as disjoint latency. |
| `async_commit_flush_us` | Separate wait for queued commit work to settle; this is not isolated Git CPU time or just the commit worker's fence hold. |

Setup JSON identifies watcher enrollment support and cold publication latency.
If enrollment fails, the harness reports that fact and measures the fallback;
it does not claim bounded incremental performance. On an enrolled fixture,
routine samples require zero canonical directory enumeration and a small
constant metadata budget, while the live full control must visit at least the
historical message count. Retain raw JSON and report medians/ranges per mode and
size. Eight samples do not establish a reliable production p99 or a
whole-server throughput improvement. System-call tracing, if used, must be
reported separately from the instrumented verifier counters.
The paired full-verification controls renew the audit timestamp, so routine
samples describe the warm interval between periodic audits. They do not measure
amortized periodic-audit cost; the audit has its own correctness regression.

The storage unit-test harness sets `ARCHIVE_INDEX_SYNC_INLINE=true`, including
on commit-coalescer workers. Its separate `async_commit_flush_us` therefore
includes inline shared-index reconciliation, while production schedules that
work asynchronously. Both actual revision arms have the same test behavior.
The publication timer stops before the flush, and the idle assertions prevent
previous commit work from carrying into the next sample. These measurements
must not be presented as production background-index synchronization timings.

## Validation status: final production check passed; native tests pending

The final frozen production source passed the native storage library check
using pinned nightly Rust 1.100.0 (`nightly-2026-08-31`, compiler commit
`90850177249efe0321573c569aec5d12b257f8d6`):

```sh
cargo check --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib
```

The final recheck completed in 10.53 seconds with maximum RSS of 587,128 KiB,
exit code 0, and no warnings, using already-built dependency metadata. It used
the original `-Zthreads=1` configuration, opt-level 0, debug information disabled,
and no compiler wrapper. The checked `lib.rs` SHA-256 is
`633c631e6a7b46a5085a9dccd4e478d92e767043d404a2754ac24b3f2c613231`.
The [receipt](verification/storage-final-production-check.json) and
[raw log](verification/storage-final-production-check.log) retain the command,
source hashes, and output. This production-only check excludes
`cfg(test)` modules: it does not type-check or execute the new publication tests.

An earlier native production-library check, before the final bounded-contention
fallback and process-interruption regressions were added, completed in
1,492.391 seconds with maximum child RSS of 6,916,476 KiB and no changed-storage
warnings or errors. Its raw log is `storage-restored-check.log`.

The full workspace passed pinned `cargo fmt --check`, and `git diff --check`
passed. The actual `canonical_watch.rs` source also passed its seven native
tests in a small independent Cargo harness using nix 0.31.3, tempfile 3.27.0,
and the pinned nightly compiler. That harness supplies only the ordinary
`symlink_metadata` wrapper used by the adapter; it does not include the storage
crate, publication code, or its dependency graph. The same adapter passed
Clippy with the workspace's pedantic/nursery settings and `-D warnings`.

Before the execution environment was restarted, repeated builds of the
unchanged locked dependency graph were killed by the shared 8 GiB cgroup while
compiling Asupersync 0.5.0. One initially idle attempt reached 4,384,784 KiB
maximum child RSS before SIGKILL. Single-job builds, disabled debug/incremental
output, a single compiler frontend, a scoped MIR optimization reduction, and
jemalloc tuning did not complete that dependency in the old environment. Disk
space also fell below 1 GiB. Those historical environment failures remain part
of the record; they are not the current native production-check outcome.

After the environment restart and successful production check, native libtest
code generation was again killed in the unchanged Asupersync dependency before
reaching the storage tests while other workloads shared the memory budget.
The hinted libtest attempt's admission window expired without launching Cargo;
that is a blocked attempt, not a compiler failure. There have been no compiler
diagnostics for the new storage test module. Any subsequent native retry must
retain the exact locked sources and feature graph and record any
dependency-scoped compiler memory options. Any resulting benchmark executable
must be identified with its actual profile; an unoptimized test build must not
be reported as a release build.

None of the new production-path correctness tests or the 5,000-, 50,000-, and
150,000-message publication benchmarks has executed yet. No storage correctness
pass count, scale latency, metadata result, fence timing, or whole-server
improvement is claimed here. The runtime integration must remain a candidate
until a suitable native runner completes those checks.

When adding those results, retain the exact tested source/binary hashes,
compiler/profile, filesystem, notification enrollment result, commands and exit
statuses, raw benchmark output, and any failed/unsupported cases. Workspace
all-target checks and Clippy gates remain distinct from this successful storage
library check and the focused storage tests. Historical bead closures and
standalone notification tests do not substitute for execution of the complete
storage publication path.
