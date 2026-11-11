# Large-mailbox qualification (GH382)

This work addresses the startup, unified-inbox, and lexical-search paths reported
in [issue #382](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/382).
The reporter measured commit `e7ffb20f` on a production-mailbox copy with 242
projects, 12,236 agents, and 203,498 messages, under a two-core CPU quota and a
4 GiB memory limit. Those measurements are the reporter's baseline, not timings
from the synthetic fixtures introduced here.

## Evidence status

**November 11 follow-through:** source packet `540db95c` plus ten overlays
(`15aa1915`, the retained 25-file manifest) passes workspace all-targets
compilation, separate default production, strict Clippy and separate portable
production, with formatting also passing. The respective remote times are
1,354.8, 2,422.7, 235.8 and 1,497.9 seconds. Check-worker manifests match all
25 files; the late additional filesystem witness for Clippy is unavailable,
so that result relies on its exact immutable-source RCH receipt. The expanded
native invocation actually runs all 1,847 cases across five binaries:
**1,829 pass, 15 fail, two time out, and one is stopped by the global watchdog**
at 1,800.031 seconds (exit 100; 12,279 skipped). The runner's 16-failed summary
includes that global cancellation. Every original 805/338 case passes, every
earlier expanded identity is present, and all 29 added controls pass. Five
stable executable hashes are retained. The native result is failed; the
compiler graphs pass. No constrained-host speedup or release clearance follows.

The passing f21 populated-worker fixture records 3,150 ms of thread CPU over
30,000 ms wall time and 48,001 ms to convergence, with a 32,394,392-byte Git
index and all 256 source/committed messages. Reporting and quota are disabled.
This is a separate bounded synthetic sample from the H13 receipt below;
neither establishes live-incumbent speedup, service soak or release send latency.

This packet includes a further caller fix: the existing byte-safe preview
prefix helper is connected after full JSON masking and newline escaping.
That preserves the original UTF-8 prefix oracle while bounding the new
allocation. All four original preview tests and eleven masking/sanitization
controls are included without changing their assertions. A later shared
canonical-identity query commit is outside this packet. Its separate
`21f4efdf` plus typed empty-vector assertion packet (`3208347e`) passes
all-targets (3,127.5 s), default production (980.1 s), portable production
(2,373.0 s), strict Clippy (1,774.4 s) and formatting. All 26 declared hashes
match each compiler worker. Its native run executes three real-pool cases:
two pass and the new canonical case fails during fixture insertion, before
the query, because the production NOCASE unique index rejects its aliases
(5.946 s, 4,021 skipped, exit 100). The stable DB executable hash is retained.
The corrected test now explicitly seeds a legacy schema without that index,
matching adjacent canonical-routing fixtures; production SQL/schema and all
14 input rows and result oracles are unchanged. Corrected `a8299ce1` passes
formatting and stock scanning. Its native rerun passes all three original cases
in 4.291 seconds (4,021 skipped), including the canonical case in 1.509 seconds;
the stable actual executable hash is retained. Its own four compiler graphs
also pass: all-targets (2,717.4 s), strict Clippy (1,131.5 s), separate default
production (2,510.9 s) and portable production (2,539.3 s). All 26 worker hashes
match each graph.
The known pinned-dependency future-Rust warning remains recorded.
The original failure remains evidence; it did not test the query.
This verifies the query and real routing/byte-bound companions, not the peer
lifecycle task's mounted retire/restart/unretire/deregister acceptance.

**Native follow-through, November 10, 2026:** the reviewed candidate based on
`ce1fac7f` plus the two-source `4037b5cf` overlay passes both workspace compiler
graphs (`--all-targets` and production `--lib --bins`), strict Clippy, standalone
formatting, and the separate portable production graph. These are compiler
results, not the constrained-host runtime qualification below. The stock scans
retain the existing query-fixture token and debug-only invariant panic alerts;
they are not uniformly green.

The November 10 expanded native execution on the intermediate five-file packet
(`8c4fa856` plus overlay `73fe2671`) actually runs 1,678 cases: **1,664 pass,
13 fail, one times out** in 1,602.838 seconds. Every original 338-case and
805-case identity executes; all 338 pass, while 800 of the original 805 pass.
The five failures are the caller-clock deadline fixture and four pool-backed
health/startup cases. Further source review repairs the fixture's clock,
preserves cached pool authority as absent directories materialize, and stops
reservation catch-up after its completed round. The expanded run also exposes
eight additional retention failures that remain open: five cleanup/publication
cases, one aggregate error-count case, one committed-Git case, and the large
empty-directory timeout. Their assertions and watchdogs remain unchanged.

That run's passing persisted-size fixture records preparation at 140,059 µs
for 100 messages and 2,557,117 µs for 10,000 messages. The integrity phase takes
137,904 µs and 2,552,678 µs respectively; archive preflight takes 869 µs and
736 µs. These warmed synthetic preparation samples include real-engine row,
body, recipient and archive-parity checks. The integrity phase includes
archive-drift and ID-floor work, so the inner cause remains unmeasured. These
are not listener-readiness timings, a constrained-host result or an
incumbent-relative speedup.

The eight-file packet `35da3ab3` passes formatting, workspace all-targets
compilation, separate production `--lib --bins`, strict Clippy, and portable
production. Its complete native execution runs **1,717 cases: 1,710 pass,
six fail, one times out**, with 12,374 skipped and 1,586.130 seconds of test
execution. All original 805 and 338 cases pass; every earlier expanded case
is present. Five stable executable hashes and all 11 worker source hashes are
retained. The unchanged publication-with-refusals case now passes, while the
aggregate-count case, four cleanup/admission cases and empty-directory timeout
remain red. The populated-worker case also fails while discovering its newly
spawned maintenance thread, after both healthy committed-byte setup checks pass.

The first test-only correction seeds the populated-worker fixture's large
index from committed HEAD, preserving mail that background index sync has not
yet copied. Added raw committed-byte checks for all 255 healthy messages before
and after setup strengthen the fixture; the original CPU, workload, deadline
and final 256-message oracles remain intact. A further test-only correction
(`b8eda4fb`) polls for the actual production thread within the existing
nine-second warmup and sleeps only its remainder. It grants no additional
warmup or watchdog time. Formatting and the complete stock scan finish;
compiler graphs and the complete native population still require terminal
qualification on those final bytes. Initial native queue and portable disk
admission refusals execute zero bodies and remain recorded separately.
Four disjoint peer commits are subsequently preserved by fast-forward to
`b0e31ce0`. Integrated qualification now uses that immutable base plus only the
thread-discovery test overlay. It retains all 1,717 earlier selected cases and
adds the incoming embedding-worker, rate-limit, ATC and OpenCode controls.
The obsolete native preparation is cancelled before any compilation or test
body; local cancellation and recovered delivery statuses are retained separately.
Another real integration gap is then found in message reconciliation: it still
uses the blocking lag sampler before selecting rows. The final runtime change
uses the existing nonblocking sampler, defers unavailable evidence through the
existing admission-contention path, and checks checkpointed workers before
restoration as well as before selection. Its regression holds the actual global
queue and checks both cursor modes, source/receipt preservation, shutdown and
real repair after release. Native assertions are pending execution.
Final qualification uses the same base plus three source overlays (`788b21ed`),
bound by an 18-file manifest. Formatting and stock scanning complete; the native
selection is admitted and compiling on vmi1149989. After its earlier queue
refusal, the all-targets retry passes on hz4 (exit 0, 1,376.2 seconds). Portable
production passes separately (exit 0, 861.2 seconds); both check-worker manifests
match all 18 declared files. The pinned proc-macro-error2 future-Rust warning is
retained from h10. Separate default production also passes (exit 0, 690.9 seconds).
Strict Clippy exposes two incoming semantic-recovery style errors, then four
incoming ATC assertion-style errors. Their fixes preserve behavior and predicates.
Native execution actually starts 1,818 cases across five binaries; its original
empty-directory timeout remains red, and an incoming ATC fixture also fails when
normal probing replaces two artificial future deadlines for never-observed
agents. That fixture now seeds active, recently probed agents and positively
checks no reprobe, preserving all 940-agent, 2,820-entry, bounded-pop and eventual
unique-review oracles. Probing remains enabled. This is a fixture correction,
not a measured runtime scheduling repair. Two further semantic test lints use
checked time subtraction and an explicit mutex-guard drop after its last use.
Peer540db95c is preserved without overlapping edits. One final OpenCode test
lint uses equivalent empty-vector equality. Final f18 binds that base plus six
overlays and adds the incoming diagnostic-retention controls to native testing.
It requires its own compiler checks and complete native execution; H13's
original failure and every retained retention failure remain evidence. H13's
populated maintenance case passes71.812s with its original workload, CPU and
convergence assertions; publication-with-refusals and repeated-hourly retention
fail in this run despite earlier passes. No green/flake inference follows.
That passing maintenance fixture records3,780ms of worker-thread CPU during
30,000ms wall time,35,268ms to convergence,32,394,392 index bytes and256 source/
committed messages. It is bounded synthetic worker evidence with reporting and
quota disabled, not an incumbent-relative speedup, service soak or the requested
release send-latency overhead qualification.
H13 terminates at its unchanged1,800-second suite limit:1,644 of1,818 selected
cases run,1,634 pass,eight fail,one times out at240s,and one is terminated by
the global watchdog;174 are not run and12,294 are skipped. All original805 and
338 cases pass, but174 earlier h10 cases remain unexecuted. Five stable actual
executable identities are retained. This is incomplete expanded qualification.
The installed runner confirms that no-capture forces serial execution. Final
f20 uses two test processes with captured success/failure output and preserves
all1,818 identities plus14 incoming diagnostic controls, normal isolation
overrides and every existing deadline/resource bound. This explicit concurrency
change is not presented as a serial pass or a service performance improvement.
Intermediate f20 also preserves fresh diagnostic-string allocations while resolving
the integrated lint error; its existing regression now checks spare tool-name
capacity as well as agent/project bounds. Unused helper removal, private-module
visibility/doc fixes and equivalent borrowed optional capacity accounting
complete that ten-overlay packet. The subsequent f21 caller repair restores
and connects the prefix helper as described above; f20's compiler results do
not qualify that later repair. The terminal f21 run above retains all 18
nonpasses, including five storage assertion failures, two per-test retention
timeouts and the coalescer's global cancellation. They are listed as open work
in the bridge plan; no load-related or flake explanation has been established.
These are self-reviewed functional repairs, not independent verification or
evidence of a startup speedup. The owned Beads remain in progress.

An earlier native execution ran 805 cases: 785 passed and 20 failed. Those
failures drove fixes to transactional snapshot comparisons, migration witnesses,
optional source identity, caller-clock deadlines, and damaged-index repair.
At that earlier checkpoint, the corrected four-crate native preparation
completed successfully and all four
executables have retained SHA-256 identities. Its first execution request is
refused before Cargo starts because active disk reservations plus the protected
floor prevent admission; it executes **zero tests**. The original case set,
four added regressions and lock-diagnostic cases still require actual execution.
An optional nextest `dev` profile now inherits the default watchdogs and
isolation rules while bounding test execution to 30 minutes, allowing build
and run to share one reservation. That combined retry is also refused before
Cargo (exit 120); the profile is parser-validated, with no runtime pass claimed.
No complete native pass, issue closure, startup speedup, or 2-CPU/4-GiB clearance
has been established. The [bridge plan](../planning/BRIDGE_PLAN_2026-09-01.md) records
the exact source hashes, receipts and outstanding acceptance.

The original integration record follows; its outages and failed attempts remain
part of the evidence history.

The implementation and regression fixtures are committed independently of their
qualification status. The execution host repeatedly disconnected during this
work. The repository's exact pinned formatter passed for the inbox, pool,
`search_response.rs`, `search_service.rs`, and final `search_v3.rs` changes.
`git diff --check` passed in the integrated checkout before the last outage.
The final legacy-migration delta in `schema.rs` and the complete server-readiness
delta could not be formatter-checked after the execution host disconnected.
Python syntax validation of the qualification runner passed before the final
required-test-list additions. Independent source/API reviews were completed.

A portable production-library compiler attempt stopped in the existing
`asupersync 0.5.0` dependency before compiling the changed code: first an allocation
failure under a protective 3 GiB address-space limit, then SIGKILL after retrying
with an 8 GiB address-space limit. The second attempt exited 101 after 12 seconds;
the host-memory watchdog did not initiate that stop. The environment did not
expose cgroup memory-event evidence, so the precise source of SIGKILL is not
established. These are build-environment results, not mailbox runtime benchmarks.
No full native compiler, test, clippy, or benchmark run had completed when this
record was written. The GitHub workflow and retained receipts, when executed,
are the authority for native results; the existence of a workflow file or a
successful commit is not a passing test.

Do not infer a latency improvement, a 30-second search-completion guarantee, or
closure of GH382 from the source changes alone. In particular, the reporter's
approximately 228-second reported readiness, 695-second usable mail routes,
10–22-second inbox, and 30-second search timeouts have not been reproduced or
remeasured by these component fixtures.

## Changes and boundaries

| Path | Change | Boundary |
| --- | --- | --- |
| JSON unified inbox | Select bounded header pages first; load alias/lifecycle rosters only for encountered candidate projects and labels only for selected messages. Keep receipt, body, roster, and label reads on one checkout and snapshot. | Explicit navigation and HTML still use full project inventory. Sparse or invisible history and equal-timestamp continuation can still require substantial scanning. |
| Lexical candidate retrieval | Use IDs and scores without the exact count, stored-body hydration, snippets, or per-hit explanations that live canonical hydration discards. | Rich index-only responses keep their behavior. SQL-planned queries retain their exact filters and ordering. |
| Search coordination | Carry the caller's cancellation and original budget through initialization, source, writer, retry, and scan checkpoints. Retain ownership until native operations return. | An already executing native SQL or Tantivy call is not forcibly preempted. Cancellation is not a substitute for a successful search. |
| Identity invalidation | A checked migration narrows identity-trigger invalidation to identities referenced by indexed messages, including orphan restoration, renames, deletes, and old/new ID changes. | Historical message rewrites and source replacement still use conservative rebuild checks. No changed-ID journal or unsafe row-count/max-ID shortcut was introduced. |
| Readiness | Sample the actual serving pool with a bounded background probe. Include pool initialization/generation, recovery exclusion, schema/message access, write-admission latches, and a successfully bootstrapped lexical generation. Delay service readiness notification until those capabilities are usable. | Liveness remains independent. Fresh index coverage and search availability are separate facts; every real search still checks source freshness and canonical visibility. |
| Background lexical repair | The server owns catch-up work, cancellation, and joins. Observed drift can be repaired without requiring a search request to reach an unready server. Idle checks use cached state. | This does not establish which maintenance lane caused the reporter's sustained 0.8-core CPU usage. |
| Startup repair failure | Failed transactional data repair prevents successful initialization and remains retryable. | The existing atomic timestamp/counter repair remains intact. Full startup repair and mandatory integrity/recovery work are not skipped on an unproven marker. |

MCP tools and mail UI currently obtain their pool configuration from the process
environment. Readiness must use that actual serving shape and refuse a mismatch
with the server's configured mailbox authority. An embedded `Config` alone does
not install a separate per-request configuration for tool archive operations.
Do not create a session-state bag for stateless requests merely to carry pool
settings: that would change identity and authorization behavior.

Serving configuration is reconstructed from a fixed number of environment
lookups; the user-config/env file is cached by generation. Matching configured
paths avoid filesystem resolution. Different path aliases may require metadata
checks, but the readiness callback does not enumerate mailbox contents.

The installed `Type=notify` service has a 300-second startup timeout. After the
HTTP supervisor begins, it sends bounded startup-deadline extensions while owned
initialization or qualifying admission work is active, for at most 30 minutes
from that supervisor's start. Extensions never emit `READY=1`. Mandatory
pre-listener preflight still uses the operator's original timeout; this change
does not extend that earlier phase. Liveness remains available while admission
is pending, and ordinary write refusal cannot falsely announce readiness.

The canonical SQLite engine reproduced the legacy v15 failure caused by
external v36 triggers during the temporary missing-`messages` interval. The
implemented repair captures exact owned definitions, restores them within the
existing transaction, and adds native rollback/retry coverage. The fixed Rust
migration itself still requires native execution.

## Regressions

The normal native tests cover:

- JSON parity for aliases, retired/deregistered direct history, shared eligibility,
  operator BCC visibility, and missing-project labels.
- Lazy roster/label selection and a concurrent mutation of roster, project label,
  body, and receipt state inside a read snapshot.
- Readiness before actual pool initialization, pending pool acquisition, pool
  generation replacement, recovery exclusion, and real archive write refusal.
- Cancellation behind actual lexical coordination locks, source replacement and
  publication guards, candidate/cursor parity, and coverage invalidation.
- Checked identity-trigger migration on the native runtime and canonical migration
  engines, including transaction/ledger rollback and conflicting schema objects.
- Startup-repair failure admission and preservation of the original source data.

The ignored inbox fixture has exactly 242 projects, 12,236 agents, and 203,498
messages. It alternates eager and candidate collectors in the same process,
compares full aggregate values, and records time/query counts for limits 20 and
1,000 before and after 6,393 retirements. A sparse `normal` filter reaches older,
widely distributed projects. Its 1 KiB synthetic bodies and relevant native
tables/indexes are not the reporter's full database or million-file archive.
A separate small fully migrated fixture exercises the actual pool/schema path.

The ignored Tantivy fixture indexes 203,498 real documents and compares rich and
lean collection of common-term, tied-score pages. It records alternating timing
samples and checks exact IDs and scores. This is a collector measurement, not a
measurement of the complete MCP search request.

## Run under verified resource limits

The runner first builds the portable production libraries and actual DB/server
library test executables outside the runtime memory budget. Choose a new output
directory; receipts and raw logs are never silently overwritten.

```bash
python3 scripts/perf/gh382_qualify.py prepare --output /absolute/path/gh382-evidence
```

On a Linux host with systemd, cgroup v2, and permission to create a transient
service:

```bash
sudo -n systemd-run --quiet --wait --pipe --collect \
  --unit=am-gh382-qualification \
  --uid="$(id -u)" --gid="$(id -g)" \
  --working-directory="$PWD" \
  --property=CPUQuota=200% \
  --property=MemoryMax=4294967296 \
  --property=MemoryHigh=3221225472 \
  --property=Nice=10 \
  --property=RuntimeMaxSec=50min \
  /usr/bin/python3 "$PWD/scripts/perf/gh382_qualify.py" \
  run --output /absolute/path/gh382-evidence
```

The run phase verifies effective ancestor CPU/memory limits, clean source identity,
Cargo.lock and native binary hashes, exact test presence, actual ignored-test
selection, and one passing test per invocation. Missing or skipped tests fail.
It retains native logs, process samples, cgroup counters, all paired inbox
scenarios, and a qualification JSON result. Formatting and clippy are separate
workflow gates and must also pass.

[The GitHub qualification workflow](../../.github/workflows/issue382-native.yml)
runs this sequence on relevant main-branch pushes and supports manual dispatch.
The normal workspace CI remains responsible for its broader production,
default-feature, integration, and platform checks.

## Remaining end-to-end evidence

Run the reporter's retained harness, or an equivalent full HTTP/MCP harness,
against the same mailbox copy and constrained host before asserting issue closure.
Compare a pinned incumbent and candidate in alternating order within the same
experiment. Record liveness, readiness, first successful mail route and search;
p50/p95/p99 inbox/search latency; successful-result parity; pool admission and
phase timings; CPU/RSS; archive visits; and rebuild counts. Include cold and warm
startup, normal writes during catch-up, sparse/no-match filters, tied timestamps,
old-message edits/deletes, source replacement, and cancellation recovery.

The native component fixtures make specific work and correctness changes
reviewable. They do not yet establish an end-to-end latency bound or explain the
reported idle maintenance CPU.
