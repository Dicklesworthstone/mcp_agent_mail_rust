# Agent Mail #374: recovery and inbox qualification

Date: November 4, 2026. The initial investigation is based on
`4b8ddf30f34a73cc2553bc95db8f906dd95328a6`. Initial preparation made no commit,
issue comment, pull request, release, or remote branch update. In a subsequent
instruction, the user authorized the two commits below directly on main through
GitHub, without a pull request. Publication receipts establish whether that
authorized update completed; publication does not establish native qualification.

The initial changes were published directly to main as
`a79b72f5676cab8b5ea2f624b84e4f99632801da` and
`aaf8c0e47f70fab1d9e44327909b18df621a964c`. The continuation below starts from
`1d12d78c26c87afecd08617f0c52d6a44127aa71`, which incorporates subsequent
upstream changes. The user again explicitly authorized direct main commits and
prohibited pull requests.

## Status and evidence boundary

### Integration on main after the November 4 merge

The investigation below records its original 0.5.0 backport and build attempts.
Merged main instead retains the application's public SQLModel 0.5.1 dependency
family, with pool and core pinned to
`c996710674d92c00d23f30fe17ecc5d9644818c7`. The `vendor/sqlmodel-pool` tree is
preserved provenance and is not selected by the root manifest or lockfile.
The native workflow now derives the pool manifest from `cargo metadata
--locked` and runs that selected source's unit tests and formatting check.
Those dependency unit tests use the dependency's development graph; the
separate real HTTP cancellation test exercises Agent Mail's consumer graph.
The workflow refuses an ambiguous pool selection and still requires a nonzero
passing test count. This wiring correction does not itself establish a
successful hosted run, large-mailbox qualification, or issue closure.

Local integration qualification subsequently ran against the frozen
`e088f5f65d62b72369136746b54ef7605bfad566` source, preserving the earlier
attempts below. Nine native inbox/header/snapshot tests passed (84.714 seconds;
3,548 excluded). All-target, separate production lib/bin, and strict workspace
Clippy checks each exited zero. Their source receipts independently matched all
3,910 input records; receipt hashes are, respectively,
`da72417b2f766d1f398cd76b50cb76587cab127572479a11a8d6667d8057583c`,
`2da289035b96e1b254204a63e8643563938f888aeff4b1c1cd308f33430f18ce`, and
`3159847a1aff3273312d3ad9cecaabafadd5c5a988eca5b0350cce9e08b6bfef`.
The native receipt is
`9df353848a8924df5267baeab381771b72efa9f5a4ab1a5c1d614e408d3c91bc`.

Review found that the continuation's `pool::tests::cancelled_` selector executed
zero initializer tests: the compiled module is named `pool_impl`. Both active
workflow commands now use `pool_impl::tests::cancelled_`, and the lane still
requires exactly two passing cases. Executing those original cases from the
same native image produced **one pass and one failure** in 12.75 seconds. The
cancelled first checkout remained retriable; the lazy-initializer waiter case
passed its cancellation/accounting checks but the subsequent fresh acquisition
timed out at its original ten-second bound. The executable SHA-256 was
`328dbda6349e2397805baf8a1013dc2c0b6aa4bb8e55ae3188474b9588a3d7a5`.
No current trace establishes that timeout's cause. This result keeps the pool
qualification task open; successful dependency unit tests do not override it.

The original two-case HTTP Cargo command subsequently passed both cases in
76.00 seconds, with 5,498 excluded. Its receipt is
`f0e0badae55cf604519ac271821b1a8eb9d743fe70ba83966187d97618653657`;
all 3,910 source records matched. This covers the original 600-message
responsiveness workload with its strengthened read requirement, and the real
HTTP cancellation/permit/pool-reuse path. The initializer failure above remains.
The later `9f3bc1b5`
shared-obligation query is a separate source snapshot and requires its own
qualification. No 25,100-message HTTP/archive result or release clearance is
claimed here. Raw terminal logs and receipts are retained under
`/tmp/am-reality-20261103-nSlwbq/merged-gh374-*`.

### November 5: shared-obligation continuation

Upstream `9f3bc1b5fe6a0c4a8942f473ad88ab5604893543` also gives the obligation
count pager a typed `i64::MIN` lower bound and a deterministic message-ID tie
break. The original 257-delivery regression now checks three bounded native
`SeekGE` pages, exact counts, and unchanged receipts/activity. The local source
adds only a redundant-borrow cleanup and three manual test-format corrections.
The first native run passed 32 cases before those formatting corrections; its
receipt `fbf01c6d3b52a7ca96f5d2e6bc72eff6f536d4157fdc234c1e1236f87a9cdf54`
remains evidence for that earlier byte snapshot.

Final formatted-source qualification completed independently:

| Check | Result | Source receipt |
| --- | --- | --- |
| Native portable `project_mailbox::tests::` family | 32 passed, 0 failed, 1 deliberately ignored scaling benchmark; 3,524 filtered; 43.06 seconds | `ac2b9c8d904f8d2e8aeabc9f079c6282ed95572327c32262bae682e8df8f156b` |
| Workspace all-target check | Exit 0 | `2a4c6603a35cc42ff0e8b257bf9d3b3a144c480643d71df5b122dcac761ce67d` |
| Separate production lib/bin check | Exit 0 | `80013ac7d3b666bf567d5a93f18c5017d60f4a1b618d6bf61a34cbc6ef4924a4` |
| Strict workspace all-target Clippy | Exit 0 | `5a8417c325fe7baa5da38cb280a7fca122147157c701a3c6ba8bf013cf64efa8` |
| Final workspace formatting | Exit 0 | Retained local format log |

All four remote receipts contain the same 3,910 source records, freshly rehashed
with zero mismatches. The native executable on `vmi1152480` is 128,102,944 bytes,
SHA-256 `73cf03e378e2ebc11ea76574826528899b18a6c6bd06946881aec5c8137ea730`.
The scan of the changed Rust file reported zero critical, 303 warning and 44
informational findings, with no failed module; it is not a warning-free audit.

The workflow's `shared_inbox_` filter did not select the obligation regression.
It now also executes that original fully qualified case with `--exact` and
requires exactly one pass with zero ignored cases. All 24 workflow shell blocks
parse. This is CI wiring and native query qualification, not a measured latency
win or a successful hosted workflow result. Worker admission/source-packing
refusals and the initial formatting failure remain retained zero-case/red
attempts. The initializer failure above still keeps its separate task open.

The original 25,100-message archive case executed on `vmi1152480` and failed:
zero passed, one failed, zero ignored, 31 filtered, 220.25 seconds. Seeding
completed with 25,100 messages and 117,178,368 database bytes in 120,236 ms.
An unnamed concurrent participant then panicked on a pool-acquisition timeout
(`archive_test_outcome`, line 253). The stop-on-unwind guard subsequently caused
the reconciliation interruption assertion to fail; that is a secondary failure.
The last progress sample reported 36 passes, 16 repairs and 43 scanned messages
after 78,204 ms of the concurrent phase. This was not a 45-minute convergence
timeout, and the case did not reach its final archive/integrity acceptance.
The participant's role and the timeout's cause remain unproved. Receipt
`828cdcc8683ba19808d02afc5db96b03d3bd7ba213118b3bc3b36096ae148bf3`
freshly matches all 3,910 source records. A later attempt to retain the exact
worker executable found its managed source directory empty; no executable
identity or diagnostic replay is claimed for that failed case. Its original
terminal output and source receipt remain retained.

The separate original 25,100-message HTTP responsiveness case passed on
`vmi1149989`: one passed, zero failed/ignored, 5,499 filtered, 57.43 seconds
(57.33 seconds in its isolated child). It recorded 418 real inbox reads,
including 186 large-recipient reads overlapping repair batches: 93 metadata
and 93 full-body reads. Eight batches repaired 16 messages; the concurrent
phase lasted 11,180 ms, maximum batch 1,695 ms, and maximum inbox read 248 ms.
Every original deadline, payload/receipt, unrelated Git staging, cancellation
drain and graceful-stop assertion was retained. Its own witness explicitly
states `full_convergence_checked: false`: this is responsiveness evidence,
not full archive convergence, a comparative speed win, or release clearance.
Receipt `d5d9c422fa87cb307c036f50a165a9aa848bb30f5fb6413f120e128bda139d13`
freshly matches all 3,910 source records. The running worker test executable
and retained local copy are both 327,234,936 bytes, SHA-256
`4fc2770248855eb554f7520d0325083bff88a521ac5b91cd28174e0295c9c70d`.
The 112-minute compilation is separate from the case's runtime verdict.

The unchanged HTTP cancellation case also passed from that retained executable
on the local host: one passed, zero failed/ignored, 5,499 filtered, 31.80 seconds
(31.78 in its child). Its real eight-message mailbox deliberately holds the
only read connection until the original request deadline. The request returned
the intended timeout at 30,001 ms; its dispatch permit drained before the held
connection was released. Pending counts before release and after follow-up were
both zero; subsequent reads took 19 and 4 ms, with one idle connection and zero
active connections afterward. The executable hash remained unchanged. This
separate real-consumer cancellation pass does not erase the earlier initializer
failure, the untraced warmup assertion (zero of five requested connections), or
the large archive failure.

The optimized portable CLI build reached its 7,200-second build transport
deadline. RCH verified that the remote process group was killed; supported
recovery acknowledged terminal exit 137. It produced no completed CLI or runtime
qualification. A new normal-cache attempt was refused before compilation by
the excluded dashboard's sibling-path containment check (exit 103, zero cases).
An explicit isolated-target retry uses two build jobs and a 14,400-second
build-only deadline; release optimization and runtime acceptance limits are
unchanged. Its verdict and the original full end-to-end rerun remain pending.
The intended control is the actual published GNU v0.3.40 executable, SHA-256
`009ee7b746daafc26ec5f1ffe7627b38a942a031710c0076de4075323de115b4`.
Fresh release metadata, signed manifest, pinned-key Minisign verification and
streaming archive-member equality were rechecked. Earlier private read-retry
control results remain labeled as that separate subject. The bounded HTTP pass
above establishes neither complete 25,100-message archive qualification nor
release clearance.

The existing manual native workflow dispatch was accepted for published source
`6e70244c` as run `41957916399`, but the latest read still reports `queued` with
no jobs. Cancellation returned HTTP 409, "not been queued yet," despite that
status. No hosted test executed or passed, and no cancellation is claimed.

The initial cancellation-accounting fix and qualification tests are on main.
Formatting, provenance, configuration parsing, and independent source review
have passed; those checks are not native execution of the pool, HTTP, or recovery
paths. Native execution evidence and its limits are recorded below. The failed
large archive attempt and the bounded HTTP pass above do not establish complete
large-mailbox qualification.
The changes do not establish production-scale qualification or resolution of
issue #374.

The maintainer's [November 4 comment](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/374#issuecomment-6632024968)
reports four DB tests, 96-message archive convergence, and 600-message HTTP
responsiveness at the base commit. These are prior maintainer results, not new
results from this investigation. That comment explicitly leaves large-mailbox
qualification and the pool's pending counter unresolved.

## November 4 continuation: timestamp seeks and current dependency qualification

### Production change and preserved contracts

The shared header query now binds `created_ts >= i64::MIN` and orders by
`created_ts DESC, id DESC`, retaining the optional inclusive timestamp ceiling,
128-row limit, and boundary-timestamp offset. The pinned runtime engine recognizes
this shape as a reverse composite-index range seek. The initial page seeks the
project prefix; continuations seek the project and timestamp ceiling instead of
walking newer timestamps again.

This is a source-supported query-plan improvement, not a measured latency win.
The 4,096-unit speculation quota, increasing page charge, exact receipt fallback,
complete-timestamp cutoff, snapshots, cancellation checks, and archive publication
rules are unchanged. Long equal-timestamp runs can still exhaust speculation and
use the exact fallback; full archive reconciliation still requires scale evidence.
The timestamp equivalence applies to valid non-null integer timestamps, not every
possible malformed-database error outcome.

The new native regression checks every candidate page against independent Rust
ordering at `i64::MIN`, negative, zero, and `i64::MAX` timestamps, with 129-row
ties and foreign-project witnesses. It checks the generated `SeekLE`/`Prev` path,
two-field continuation keys, absence of a sorter/history-start instruction,
complete incumbent row-set equivalence, and query-only transaction cleanup.
Existing shared-inbox paging and concurrent peer-write hooks were updated to the
new SQL text so their assertions continue to exercise the intended operations.

The published `fsqlite-vdbe 0.4.13` crate was downloaded and verified against
Cargo.lock checksum
`1ebdb969e559db53f5ff2fed49fdfc4cd651a3fd50e6eefc3350ea46a708d125`.
Its VCS revision is `e4137af304002ac0b57a948c9e0005f9161475d7`; the
`src/codegen.rs` blob is `b9bd72adadd70ecd3a01846a518ad03467a03d08`.
The reverse composite-range implementation is therefore in the actual published
runtime engine, independently of the separate patched Beads 0.3.18 engine.

### Cancellation dependency and CI correction

Current main selects `sqlmodel-pool 0.5.1` and `sqlmodel-core` at immutable Git
revision `c996710674d92c00d23f30fe17ecc5d9644818c7`. The vendored 0.5.0 copy is
historical evidence and no longer provides the application's runtime pool.
Source review confirms the selected pool owns the pending counter through
`WaiterGuard`, factory reservations through `ActiveSlotGuard`, and connections
being validated/retired through `ActiveConnectionGuard`. Its outer acquisition
drops those owned operations before reporting cancellation or timeout. These
findings identify cleanup mechanisms, not a newly executed no-leak proof.

The CI pool step now runs the two existing native Agent Mail initializer
cancellation/reuse tests using the application's locked dependency graph and
records the selected pool's manifest/lock identity. The unchanged HTTP cancellation
test separately requires queued-request cleanup, dispatch release, metadata/body
reads after timeout, zero pending counts, reusable capacity, and unchanged receipts.
The workflow also selects the adjacent shared-inbox tests and requires the storage
large-fixture JSON witness to say `large_mailbox: true` and `messages: 25100`.
All existing large-workload, integrity, latency, production, and strict Clippy
gates remain. Historical vendor formatting is checked directly with rustfmt.

### Continuation validation

| Check | Observed result |
| --- | --- |
| Changed Rust files and historical pool rustfmt | Passed |
| `git diff --check` | Passed before executor loss |
| Workflow YAML and all 23 shell blocks | Parsed; shell syntax checks passed |
| Published runtime engine checksum/VCS and independent source reviews | Passed source/provenance checks |
| Focused native header-seek regression | No test result observed; executor disconnected during dependency compilation |
| Workspace `cargo fmt --check` | Attempt interrupted, exit 130; no completed formatting verdict |
| All-target/production workspace checks and strict Clippy | Not executed in this continuation before executor loss |
| 25,100-message archive and HTTP variants | No new execution result; production-scale qualification remains open |

The standard native attempt started at 22:31:09 UTC and was interrupted during
dependency acquisition (exit 130). A retry started at 22:35:42 UTC with Cargo's
`-Zgit=shallow-deps`, the pinned compiler, one build job, no incremental compilation,
and zero debug information. It downloaded the locked crates and reached dependency
compilation. Its terminal status and resource summary were lost when the executor
reported `exec-server transport disconnected`; the environment then became
unavailable. A configured 1,200-second build budget is not an observed timeout
result. No native case count, workload latency, Git-lock contention rate,
reconciliation throughput, or workload resource peak is claimed.

The current application-graph commands are:

```sh
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable pool_impl::tests::cancelled_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable gh374_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable project_mailbox::tests::shared_inbox_ -- --nocapture --test-threads=1
```

The unchanged native HTTP cancellation and opt-in large-workload commands below
remain required. Workflow configuration is not evidence that those jobs executed.

### Exact continuation commit plan and publication

1. `perf(inbox): seek header pages by timestamp and id (#374)` — published directly
   to main as `145d4286ba453f2b8a3a22e400b082ad844d6cdf`, parent
   `1d12d78c26c87afecd08617f0c52d6a44127aa71`. Files:
   `crates/mcp-agent-mail-db/src/sync/inbox.rs` and
   `crates/mcp-agent-mail-db/src/project_mailbox.rs`. GitHub tree
   `cc63d2ed471ae0dea76d7a11caae3f1585021619` exactly matches the reviewed
   local tree.
2. `ci(gh374): qualify the selected pool and shared inbox paths` — this evidence
   update accompanies the CI correction. Files:
   `.github/workflows/issue374-native.yml`,
   `docs/evidence/gh374/VALIDATION.md`, and
   `docs/evidence/gh374/validation.json`. Publish with a checked non-forced
   fast-forward from the first commit; retain intervening work if main moves.

Neither commit claims issue closure or successful native production-scale
qualification. No pull request is part of this plan.

## Initial implementation changes

### 1. Cancel-safe pending-request ownership

The published `sqlmodel-pool 0.5.0` increments `waiter_count` before awaiting a
connection notification/timer and decrements it afterward. Dropping the acquire
future during the await bypasses the decrement. A private `WaiterGuard` now
owns precisely that increment. `AcquireAction::Wait` holds the guard, whose
destructor decrements the count on wake, timeout, cancellation, unwind, or hard
future drop. The two former manual decrements are removed.

No connection slot is reserved in this wait state. The guard does not return a
connection, change active/total capacity, alter acquisition deadlines, or touch
transactions, recovery admission, source-generation checks, archive publication,
or receipt state.

This ownership strategy already exists in SQLModel's unreleased
[`ee08466f00162ea21c0f9cf05da1f05f04d44758`](https://github.com/Dicklesworthstone/sqlmodel_rust/commit/ee08466f00162ea21c0f9cf05da1f05f04d44758).
That commit also contains a substantially broader acquisition-deadline rewrite.
The local change backports only waiter accounting to the exact published 0.5.0
source, rather than importing those unrelated changes.

The reproducible copy is in `vendor/sqlmodel-pool`. Its README records the
archive checksum, original VCS commit, upstream guard commit, and source delta.
The archive SHA-256 is
`d090572646f14c79363c6a78fe073afdb2213b77421b1815b31a6761e1f3ffa2`, matching the
original root lockfile. The published VCS commit is
`12d490b22ce3a82533591b5b39d2318b21da1f0f`; the original license blob is
`3d414da6ff83defe084707be6030866e8a7fb794`.

In the original backport, all published files other than `src/lib.rs` and the
extended README are byte-identical. The upstream license is included unchanged.
Its proposed root Cargo used a local `[patch.crates-io]` for this one crate.
Among that root lockfile's 868
packages, only the pool's registry source/checksum are removed; package versions
and dependency edges are unchanged. That proposed dependency selection was
superseded at integration as described above; it is not merged main's graph.

### 2. Regressions distinguish accounting from capacity

Three dependency tests use the actual pool and Asupersync notification code,
with an in-memory test connection implementation:

- Repeatedly drop 32 queued acquisitions while preserving one live waiter and
  the caller holding the only connection. Assert exact pending, active, and
  total counts.
- Independently of pending statistics, require a new notification after the
  holder returns, acquisition by the surviving waiter, and subsequent fresh
  acquisition without another factory call. This test can pass on the old
  implementation even when the accounting tests fail.
- Close the pool after dropping one queued caller. Require the other waiter
  to observe closure, pending to drain, and the foreign holder to remain owned
  until it is returned.

A control copy contains the unmodified published production code plus the same
three test bodies. Its intended red/green comparison must be executed before
claiming that the distinction has been established experimentally.

The existing native HTTP cancellation test now records dispatch drain and the
pending count while the single contended lease remains held. It then releases
that lease, requires successful body and metadata reads within the existing
30-second deadline, and checks one total/idle connection and zero active
connections. Finally it requires both pending snapshots to equal zero. This
retains the accounting gate while allowing an unfixed control to collect
post-timeout usability evidence before failing.

### 3. Large-mailbox evidence and an inactive-agent workload

The archive test now reports seeded/converged phases, periodic progress,
scanned and repaired work, transient deferrals, maximum retry debt, budget
yields, first-repair time, maximum batch time, and live Git-lock ownership
checks. It separates convergence time from post-convergence integrity and repeat
verification. Per-reader latency histograms are bounded and use milliseconds
rounded upward before reporting sample p50/p95/p99/max values.

The HTTP test requires at least 32 successful reads, giving at least eight
samples per large/small inbox and metadata/body combination across the entire
observation window. It separately reports latency distributions for requests
whose lifetimes overlap a repair-positive batch. At least one overlapping
large-inbox metadata request and one overlapping large-inbox body request are
required; the eight-sample minimum does not apply to the overlapping subset.
Any subset with zero samples reports null quantiles and maximum latency.
Per-batch work, overlap counts, maximum time across all batches, maximum
repair-positive batch time, and `full_convergence_checked: false` are also
reported. These are observed sample quantiles, not production percentile
estimates. HTTP child output is retained
until the child finishes; archive progress is streamed through inherited stderr.

The additional DB test seeds 25,100 GreenStone receipts followed by 1,024 newer
project messages to RedFox. It requires the exact five IDs
`25100, 25099, 25098, 25097, 25096`, with accurate, understated, and missing
count hints. It records the eight/one/one speculative header pages and the
99-page exact receipt fallback. Cancellation during that fallback must stop
further read queries, release the savepoint, preserve query-only mode, and permit a fresh
exact read on the same connection. This fixture uses the native in-memory
database engine, metadata-only reads, and tiny message bodies, without HTTP or
concurrent reconciliation. It qualifies fallback exactness and measured query
work; it does not establish production mailbox latency or resource consumption.

All original safety and timing assertions remain: complete recovery, durable
retry restoration/drain, live Git ownership, unrelated staging preservation,
exact archive bytes and BCC handling, unchanged source messages and receipts,
integrity checks, no duplicate repeat commits, and bounded inbox deadlines.

## Findings that require careful interpretation

### The leaked counter has a secondary operational consumer

The issue comment described pending accounting as statistics-only. Source
inspection identifies an additional consumer:
[`HealthSignals::from_snapshot`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-core/src/backpressure.rs)
suppresses cumulative acquisition p95 only when both active and pending counts
are zero. A stale pending sample can keep historical latency in health
classification. If health is Red and `BACKPRESSURE_SHEDDING_ENABLED=true`,
eligible lower-priority tools can be rejected. Per-project inbox reads are not
in the shedable set.

This is a source-supported possible secondary effect, not a reproduced
production shedding incident. It does not imply that the pending counter
controls pool admission: capacity is based on total connections; actual waiting
is owned separately by `Notify`. Existing guards also own slots/resources
during pending creation, validation, and retirement. The native liveness tests
are needed to distinguish these mechanisms experimentally.

The backport does not import SQLModel #43's broader deadline handling for
stalled/uncooperative factories or checkout pings. Hard-drop ownership and
independent driver deadlines are separate guarantees.

### Full missing-archive repair has quadratic identity work

For each missing message bundle,
[`identity::prepare_committed`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-storage/src/recovery/message_reconcile/identity.rs)
checks existing committed canonical identities. The
[working-tree collision check](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-storage/src/recovery/message_reconcile.rs)
then scans existing canonical frontmatter under publication/project locks.

An idealized empty-archive model with no retries and one successful publication
per message implies `25,100 * 25,099 / 2 = 314,992,450` prior-message visits in
each full identity scan. This model excludes directory entries, staged-index
inspection, and repeated attempts. Git identity verification materializes and
hashes full blobs, so the 3 KiB bodies alone imply about 968 GB of logical blob
bytes processed in that model. These are source-derived operation/byte counts,
not measured elapsed time, physical disk I/O, or memory use. A mostly complete
production archive follows different paths; already committed bundles with all
message copies present can avoid full history scans.

No unsafe cache shortcut was added. Existing archive epochs are observational,
not publication authority; lock generations are process-local. A cached OID
summary can also conceal an object that was removed or corrupted after warming.
Any optimization needs negative controls for malformed/duplicate old-shard
evidence, same-length edits with preserved mtime, symlink replacements,
unavailable/corrupt Git objects, namespace drift, budgets, and cancellation.

### Bounded speculation is not a constant-work inbox guarantee

The existing dense recent-message fixtures find five recipient messages in the
first header page. A recently inactive agent can exhaust speculation and then
read all of its receipts to preserve exactness. The added workload explicitly
measures this fallback; it must not be described as constant-work merely because
the returned limit is five. Synchronous SQL checkpoints also do not preempt a
single long-running engine query.

## Native fixture realism limits

Even a successful opt-in run has a defined scope:

- Storage proves full convergence and direct query-only inbox responsiveness;
  HTTP transport is supplied by the separate server test.
- The HTTP test reads a 25,100-message DB during 8–64 repair batches (at most
  256 repaired messages). It does not prove late-stage HTTP latency or full
  convergence.
- Both tests hold a real `git update-index --index-info` writer's index lock
  continuously. Intermittent release and reacquisition against a fully
  populated 25,100-message index remain untested.
- The storage writer updates an agent heartbeat and an archive note; it does
  not add sustained new-message arrivals.
- Cursor restart occurs inside the same process, not through killing and
  restarting the whole daemon.
- The storage test allows 45 minutes for convergence and another 45 for repeat
  verification. Test RSS includes full-mailbox integrity witnesses and other
  fixture overhead.

## Validation environment and current results

Host: Ubuntu 24.04.3 LTS, Linux 6.18.57, x86-64; cgroup CPU quota 8 CPUs, memory maximum
8,589,934,592 bytes, no swap. Pinned compiler:
`rustc 1.100.0-nightly (908501772 2026-08-30)`, toolchain
`nightly-2026-08-31`. Native build attempts use one Cargo build job, incremental
compilation disabled, and dev/test debuginfo disabled.

The workspace initially lacked Rust. The pinned compiler was installed. A
normal Cargo fetch spent several minutes processing a 440 MiB Git-history
download; the exact immutable FrankenSQLite revision was then fetched with a
shallow Git CLI fetch. The broader graph subsequently reached Rust dependency
compilation. The shared cgroup reached its memory ceiling and recorded OOM
events; these observations must not be attributed to Agent Mail runtime use.

The first offline pool control attempt exited 101 after 293.522 seconds waiting
on the package cache and encountering an unavailable `aes 0.9.3` download.
An isolated control build then expired after 600.953 seconds while compiling
dependencies, before any pool test ran. A warmed offline retry also expired,
after 1,200.467 seconds, during dependency compilation. Maximum single-process
child RSS was 37,120, 384,016, and 301,876 KiB respectively. These are build
observations, not mailbox workload consumption. The machine-readable companion
records each attempt's command, exit, wall time, CPU, and RSS scope.

A subsequent bounded candidate `cargo check --locked --offline --lib --tests`
was started before the executor became unavailable. After recovery, its log was
empty and no metrics/exit receipt existed. Its outcome is unknown; it supplies
neither a successful type check nor a test result. A final control attempt after
environment recovery expired after 300.190 seconds during dependency compilation,
with 327,764 KiB maximum single-process child RSS, 42.966 seconds user CPU, and
14.483 seconds system CPU. It also executed zero tests. None of these attempts
reached a pool regression test body. No further native run is pending in this
qualification record. Aggregate process-tree RSS is not claimed: the executor's
`/proc` PID-namespace view did not support a reliable sampler.

| Check | Current result |
| --- | --- |
| Workspace `cargo fmt --check` | Passed |
| Standalone vendored crate formatting | Passed |
| `git diff --check` | Passed |
| Cargo manifests/locks and workflow YAML parse | Passed |
| Locked, offline workspace `cargo metadata --no-deps` | Passed; manifest/workspace inspection only |
| Exact archive checksum, unchanged source files, license blob | Passed |
| Root dependency delta: same 868 packages/versions/edges | Passed |
| Pool unpatched accounting/liveness controls | Not executed; four attempts ended before test execution |
| Patched pool tests | Not executed |
| Candidate standalone `cargo check --lib --tests` | Unknown after executor loss; no exit receipt or compiler output |
| Native Agent Mail DB/storage/HTTP regressions | Not executed in this workspace |
| Opt-in 25,100-message archive and HTTP tests | Not executed; production-scale verdict open |
| Workspace type checks and strict Clippy | Not completed |

## Reproduction and acceptance commands

Use the repository-pinned toolchain and preserve `--locked`. Run commands
sequentially on a host with sufficient memory. Capture compiler identity, exit
status, the emitted JSON, wall time, CPU, and peak RSS separately from builds.
Do not accept zero selected tests as qualification.

```sh
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable pool_impl::tests::cancelled_ -- --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable gh374_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable sync::inbox::tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable project_mailbox::tests::shared_inbox_ -- --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::message_reconcile::database:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::message_reconcile::cancellable_tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::reservation_reconcile::repair_lock::fence::tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable archive_index_sync_ -- --nocapture --test-threads=1

AM_TEST_GH374_LARGE_MAILBOX=1 cargo test --locked -p mcp-agent-mail-storage --test it --no-default-features --features portable archive::gh374_reconciliation_converges_with_live_git_writer_and_inbox_readers -- --exact --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-server --lib --no-default-features --features portable tests::issue374_http:: -- --nocapture --test-threads=1
AM_TEST_GH374_LARGE_MAILBOX=1 cargo test --locked -p mcp-agent-mail-server --lib --no-default-features --features portable tests::issue374_http::gh374_http_inbox_stays_responsive_during_reconciliation -- --exact --nocapture --test-threads=1

cargo check --locked --workspace --all-targets
cargo check --locked --workspace --lib --bins
cargo clippy --locked --workspace --all-targets -- -D warnings
```

## Initial authorized main-branch commit plan (completed)

The user's earlier follow-up authorized these two commits directly on main,
without a pull request. They were published as
`a79b72f5676cab8b5ea2f624b84e4f99632801da` and
`aaf8c0e47f70fab1d9e44327909b18df621a964c`, with their trees checked against
the reviewed local files. The historical plan below records their scope;
subsequent dependency selection is described in the continuation above.
Publication does not change validation results or remove qualification gaps.

1. **`fix(pool): reclaim cancelled acquisition waiters (#374)`**

   Files: `.gitignore`, `Cargo.toml`, `Cargo.lock`,
   `.github/workflows/issue374-native.yml`, and these nine vendor files:
   `vendor/sqlmodel-pool/.cargo_vcs_info.json`,
   `vendor/sqlmodel-pool/Cargo.lock`, `vendor/sqlmodel-pool/Cargo.toml`,
   `vendor/sqlmodel-pool/Cargo.toml.orig`, `vendor/sqlmodel-pool/LICENSE`,
   `vendor/sqlmodel-pool/README.md`, `vendor/sqlmodel-pool/src/lib.rs`,
   `vendor/sqlmodel-pool/src/replica.rs`, and
   `vendor/sqlmodel-pool/src/sharding.rs`.

   Includes the narrow ownership fix, three dependency regressions, provenance,
   original license, unchanged dependency versions, and standalone native test
   and formatting gates. No release version bump or broad SQLModel upgrade.

2. **`test(gh374): strengthen large-mailbox recovery qualification`**

   Files: `crates/mcp-agent-mail-db/src/sync/inbox.rs`,
   `crates/mcp-agent-mail-server/src/tests/issue374_http.rs`,
   `crates/mcp-agent-mail-storage/tests/archive.rs`,
   `docs/evidence/gh374/VALIDATION.md`, and
   `docs/evidence/gh374/validation.json`.

   Includes inactive-inbox exact fallback/cancellation, HTTP post-timeout
   usability and latency distributions, archive progress/latency observability,
   and the exact executed-versus-unverified validation record. Does not declare
   #374 resolved or change recovery-safety contracts.
