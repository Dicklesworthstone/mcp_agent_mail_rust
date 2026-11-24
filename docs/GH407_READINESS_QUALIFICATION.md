# GH407 independent readiness qualification

This is an independent qualification harness for a supplied Agent Mail binary.
It complements the supervisor change in `c56722595d20182d37b0fced7313b70307940067`
and its real-supervisor regression. It does not alter the supervisor or any
production Rust code.

Initial reviewed source: `6eb6264d5d6becc2a7609d795c7209c88aac9d55`, rechecked against
`462914b703b93ea0d3d070dccf990e225a0a189a` before publication. The intervening
changes do not alter the supervisor, native readiness tests, or pinned
dependencies. Current-main native compilation and execution of the existing
Rust regression bodies remain **unverified**. The completed historical native
control below independently reproduces the v0.3.41 bug and validates the real
database-fault adapter; it does not qualify the current supervisor fix. Earlier
harness-only and incomplete native attempts are retained with their original
scope and measurements.

This contribution adds qualification tooling and documentation. It leaves
production Rust, dependency manifests, and Beads status unchanged.

## Qualification files

| File | Purpose |
| --- | --- |
| `scripts/qualify_health_idle.mjs` | Linux black-box runner using Node built-ins; no package installation or native source hooks. |
| `tests/test_qualify_health_idle.mjs` | Adversarial model tests of the runner's exact orchestration and verdict logic. |
| `tests/test_qualify_health_shutdown.mjs` | Real Linux process controls for requested shutdown, earlier exit, and version/SQL helpers that leave background children. |
| `tests/fixtures/gh407_agent_mail_surrogate.mjs` | Clearly labelled process/HTTP/system-SQLite surrogate for testing the runner's real adapter. |
| `scripts/ci/qualify_issue407.py` | Exact-source native qualification driver with owned process cleanup and retained failure receipts. |
| `tests/ci/test_qualify_issue407.py` | Adversarial source, artifact, verdict, and real process-ownership controls for the native driver. |
| `.github/workflows/issue407-native.yml` | Runs controller controls before the native regressions and same-build binary qualification. |
| `docs/GH407_READINESS_QUALIFICATION.md` | Reproduction, evidence interpretation, source findings, and remaining qualification work. |

The harness requires Linux `/proc`, Node 20 or newer, and `getconf`. The optional
surrogate uses `node:sqlite` and was exercised with Node 24.22.3. It is not
FrankenSQLite, Asupersync, or an Agent Mail build.

## Original measured harness validation

The executable harness validated by these runs, published in `bc407d6b`, had SHA-256
`0a267ad29966761cc94c3f7bcad7dd06d067dceea31791459c8217e982e30ce4`.
The process surrogate source SHA-256 is
`4d7284ffbd6cd532b995623d7437a7f173933ef31fc3611a7414433654f2fd60`.
That integration audit verified that the files remained identical to the
frozen copies used by each run. Earlier source receipts are retained separately.

| Validation | Measured result | What it supports |
| --- | --- | --- |
| Exact orchestrator model suite | 28/28 passed | Verdict logic rejects deliberate failures and treats missing controls as incomplete. |
| Healthy real-process surrogate | 19/19 checks passed; exit 0; 117.248 seconds | Linux process, HTTP, CLI helper, resource and restoration adapter works against this controlled surrogate. |
| Startup-only refresh surrogate | Expected exit 1; first idle response 503/stale; nine failed checks; 74.329 seconds | The runner detects the original idle-refresh failure and preserves the failed first response. |
| Always-ready surrogate | Expected exit 1; three failed outage/stale/shutdown setup checks; 117.213 seconds | The runner rejects false readiness despite independently established database errors. |
| Independent request-log audit | Zero HTTP requests in 10 healthy, 5 old-idle, and 10 false-ready declared idle windows | The harness does not accidentally refresh readiness during its idle windows. |
| Native Agent Mail tests and binary runs | Zero | No native qualification claim. |

In the final healthy surrogate run, the three first idle requests took 24.28,
3.19, and 4.98 ms and returned 200. All 16 concurrent first requests passed.
The 30.001-second idle soak averaged 0.0010 CPU cores. During the outage, four
failed probes completed in 8.070 seconds, below the predeclared maximum of six;
average CPU was 0.00372 cores. Settled threads remained 7, descriptors remained
19, and RSS went from 43,982,848 to 45,834,240 bytes. Both server lifetimes exited
0 without forced termination, left no process-group members or listeners, and
had quiet database/archive files during the one-second post-exit observation.

The stopped-process challenge returned `database_probe_failed`, so the actual
surrogate run did **not** directly exercise the stale-response branch. The
separate adversarial model exercises stale-success rejection. Native stale and
pending-probe acceptance still requires the existing native tests.

Two targeted adapter controls on the immediately preceding harness revision
also passed: a SIGTERM-ignoring surrogate required forced SIGKILL after its
two-second diagnostic grace (3.016 seconds including the quiet window), and
interrupting a confirmed suspended healthy surrogate resumed and stopped it
cleanly in 1.026 seconds. Those receipts are bounded surrogate process-control
evidence and remain labelled with their own source/version scope.

## Shutdown qualification correction, November 24, 2026

Independent review found that the original shutdown oracle could accept exit 0
when the server had already exited before `stop()` sent any signal. Absence from
`/proc` also counted as an unchanged identity. This was a harness false pass,
not an observed Agent Mail shutdown failure.

The corrected adapter records `already_exited_before_shutdown`, `sigterm_sent`,
and the process state observed immediately before shutdown. A pass now requires
a live process with the original start identity and a successful SIGTERM syscall,
in addition to the existing clean exit, closed listener, empty process group,
and quiet-file checks. The signal helper rechecks the leader and returns false
on `ESRCH`. Earlier exit still runs cleanup and produces a receipt; subsequent
`stop()` calls retain that original receipt. A successful signal syscall and a
live-process witness do not by themselves prove which event caused an arbitrary
native process to exit.

The corrected harness SHA-256 is
`f04b1c3be0d6a5cbb1a3cbfe192bf78ddb5cbd7e19789cc0ebc3aec9f53cfbe5`.
The tests ran on Linux with Node 24.22.3:

| Validation | Result |
| --- | --- |
| Exact orchestrator models plus focused real-process controls | 32/32 passed, zero skipped, in 2.732 seconds: 30 model cases and two actual Linux child-process cases. |
| New adversaries against the original `c2944c1c` harness | Both failed their assertions as expected: the old harness incorrectly returned PASS for an earlier exit 0 and for an unsent SIGTERM. |
| Live-child adapter control | The child independently wrote a SIGTERM witness; receipt records live original identity, signal sent, clean exit 0, closed listener, and quiet files. |
| Earlier-exit adapter control | The child voluntarily exited 0; its `/proc` entry disappeared before `stop()`. Receipt records earlier exit and no SIGTERM while retaining successful cleanup evidence. |
| Native qualification driver controls | 24/24 passed in 13.110 seconds; includes real timeout, interruption, and detached-descendant cleanup. |
| Workflow structure, pinned action references, four embedded shell blocks, and whitespace check | Passed. |

The two new adapter tests also verify that repeated shutdown calls return the
same receipt. CI runs them with the existing model suite before native
qualification. These measurements validate the correction; they do not repeat
the original full HTTP/SQLite surrogate qualification or execute any Agent Mail
native regression. The original longer runs above remain bound to their original
source hash.

## Helper process containment correction

Two actual-process adversaries demonstrated that the previous `f04b1c3b` adapter
could accept exit 0 and valid helper output while a child remained alive in the
helper's process group. One `--version` helper was admitted and one SQL helper
returned `ok: true`; both children were still live after `io.cleanup()`. These
are demonstrated harness false passes, not observed Agent Mail child leaks.

Command completion now records the unexpected process-group members, performs
bounded TERM/KILL cleanup, and preserves the original violation after cleanup.
The cleanup waits are capped by the existing command timeout plus four-second
terminal deadline. Version, clock, and SQL admission require an explicitly clean
process group, complete output, and no timeout. A SQL helper process violation
is classified as `process`, so it cannot satisfy the database/schema-failure
witness. Existing server signal and process-identity checks remain in force.

The corrected harness SHA-256 is
`c27452ad5a7f8d0492febc03a9da0daea278f88dfc0cc875f5c3403ae98dc29f`.
On Linux with Node 24.22.3, the full model and process suite passed **34/34**,
zero skipped, in **5.722 seconds**. Against the frozen previous adapter, both
new regressions failed as expected; the test then terminated the recorded
same-birth children. Against the correction, neither test needed emergency
cleanup. The version helper was rejected in 0.306 seconds. The SQL child ignored
SIGTERM, required SIGKILL, and was no longer live when the call returned after
1.274 seconds. Its brief zombie state remained visible in the receipt, which
correctly retained `process_group_clean: false`.

These controls measure the helper's original process group. They do not prove
containment of arbitrary children that create a new session and escape that
group. The outer native driver provides additional descendant tracking; finite
process observations remain separate from absolute lifetime guarantees.

### Historical native release preflight and execution

The official v0.3.41 Linux GNU archive was downloaded and verified against its
signed checksum manifest. Both the manifest signature and authenticated trusted
comment passed under the installer-pinned key ID `1BBD79B28BF718D0`; modified
manifest and modified-comment controls were rejected. The archive SHA-256 was
`962a34d0aba41d08432e6873cb8bf96c7a547bfad7ac273c7dabc499639b86f4`.
The extracted `am` SHA-256 was
`0427aeb52d0982f1d903c97f4cc15493afb8680b80433ab33fa8a9bac9d4c1ed`.

Three guarded native CLI invocations passed: `am --version` reported `am 0.3.41`,
and `serve-http --help` plus `tooling db-exec --help` confirmed the required
flags. Each completed in approximately 0.104 seconds, without forced cleanup,
remaining descendants, or database creation. The signed comment identifies the
release and checksum file, not a source commit; the observed tag separately
resolved to `6eca1c4dfeb9b1eedb2ff3ebb06faef23018342c`. Its published portable
feature selection also differs from the default-feature current-source lane.

That first preflight attempt ended before any HTTP server started: `/tmp` free
space fell to 510,918,656 bytes, below the unchanged 2 GiB reserve, and the
executor became unavailable. The extracted binary was absent after restoration.
The first attempt's measurements remain transcript evidence.

A subsequent attempt reauthenticated the release and ran the unchanged
`f04b1c3b` harness against a fresh synthetic mailbox. Times below are relative
to the harness orchestration start:

| Historical native observation | Measured result |
| --- | --- |
| First actual HTTP response | 503, `database_initializing`, at 2.8895 seconds. |
| Startup readiness | Later responses included `database_probe_pending` and `lexical_unverified`; these are not the reported idle-staleness failure. |
| First ready response | HTTP 200 with all three capability flags true at 11.7873 seconds. |
| Fresh MCP database operation | `ensure_project` succeeded at 11.8539 seconds. |
| Idle intervals, schema faults, and recovery cases | Zero executed; no `database_probe_stale` response observed. |
| Shutdown after interruption | SIGTERM sent, exit 0, no forced kill or surviving process-group members, listener closed, files quiet. |
| Final binary identity | Matched the authenticated executable hash. |

The outer runner interrupted the command after **18.3736 seconds** because
shared free disk crossed its 2 GiB reserve. Its receipt records exit 2,
`INCOMPLETE`, `termination_reason: disk_budget`, no forced termination, and no
remaining processes. The harness also retained `INCOMPLETE`: baseline burst and
warmup had not completed, despite the observed 200 response and successful MCP
operation. No retry was admitted below the reserve. At inspection, the entire
synthetic fixture allocated 864,256 bytes; the much larger reduction in shared
free space remains unattributed.

The raw native report SHA-256 is
`ea28418d8deed2a5049a9c8571a6dbbe209fd8f979648b810833b7dad6e46152`.
Its raw report, logs, launch settings, resource samples, signed release
manifests, synthetic fixture and controller receipts were retained in a verified
80-file evidence archive. The auxiliary acquisition controller's first summary
hit an `AttributeError` on a null pre-listener response after the harness had
returned; a separate null-safe inspection preserved the original result and
logs. This establishes historical native startup, MCP compatibility, and clean
shutdown under interruption. It establishes neither an idle reproduction nor
outage/recovery/resource-bound qualification, and is not a current-main native
pass. It also predates the `c27452ad` helper-containment correction.

## Completed historical native negative control

A restored executor subsequently completed both the exact reporter control and
the full unchanged harness (SHA-256 prefix `c27452ad`) against the same
authenticated v0.3.41 binary. This establishes a native negative control for the reported bug and
validates the real adapter's database-fault mechanism. It does not qualify the
current supervisor fix or a current-main build.

The reporter control obtained 200/ready, then issued no client HTTP, MCP, or
database request for **8.00119198 seconds**. Its first `/health` response was retained
before any further operation: **503, `database_probe_stale`, all three capability
flags false**, in **3.606357 ms**. A new-key MCP `ensure_project` immediately
afterward succeeded with HTTP 200 in **22.712463 ms**. No readiness retry was
made before or after that database operation. The controller completed in
22.511536 seconds and recorded clean SIGTERM shutdown and unchanged binary
identity. This directly demonstrates a stale health failure while the serving
database remains usable.

The full harness then completed in **107.637374 seconds** externally
(106.646850277 seconds inside orchestration), with exit 1 and an overall
**FAIL**: nine passing and nine failing case records. These are raw outcome
counts, with precondition failures and a no-op restoration described below.
The outer runner recorded
no resource termination, no forced cleanup, and no remaining processes.

| Full historical run | Measured result |
| --- | --- |
| Initial baseline | Passed, including a fresh MCP project operation. |
| First request after eight seconds idle | 503 with `database_probe_stale`. |
| Concurrent first requests after another eight-second idle | All 16 were 503/stale; response latencies were 8.894–36.466 ms. |
| Thirty-second idle soak | The first health response remained stale. |
| Genuine database fault | Same-engine table rename produced an independently attributed fresh-key MCP database error in both fixture lifetimes. |
| Both actual inverse renames | Preserved the single project row and schema-object identities. |
| Recovery without readiness polling | First response after eight seconds remained stale. |
| Both server shutdowns | Exit 0 after SIGTERM, no forced kill or survivors, closed listener, quiet files. The second cleanup occurred before schema restoration, while the fault remained active. |
| Five settled resource samples | Threads 47 to 47, descriptors 54 to 56, RSS 134,983,680 to 138,317,824 bytes; the configured finite-growth check passed. |
| Final executable identity | Matched the authenticated release hash. |

The real fault witnesses returned correlated MCP results with `isError: true`,
`DATABASE_ERROR`, `Query error: no such table: projects`, and classification
`schema_drift_or_missing_tables`. Their HTTP transport returned 200; the MCP
result established the database failure. These observations validate the chosen
fault against live FrankenSQLite connections. Pool exhaustion and blocked
synchronous database operations require separate controls.

During those outages, health still returned **503 with all capabilities false**.
The failing assertion required a fresh `database_probe_failed` result but
observed `database_probe_stale`. Despite the generic assertion's wording about
fail-closed reporting, this was a freshness/reason failure, not false readiness.
The observed failures match the historical absence of continued probe refresh.

Coverage remains explicit: idle cases two and three failed their healthy
precondition before another eight-second wait. The suspended stale-success
challenge also failed its healthy precondition, and its recorded restore pass
was a no-op (`needed: false`). Outage liveness and fresh failed-probe cadence/CPU
checks were not reached after the stale-outage assertion failed. The finite
resource pass does not establish lifetime bounds or the current fix's behavior.

Both runs retained the unchanged 2 GiB disk and 1 GiB available-memory guards.
The raw reporter report SHA-256 is
`b34e7a4d2df02616139ff0a0b17e829bcb12c58163ea46d54b76cd2f84bc06f8`;
the full report SHA-256 is
`55cf9628b4e7201a3637b14451ed3c016f83f42a0b3f11f5c0f5908fd2584c92`.
The completed evidence archive retains 263 evidence files, including reports,
append-only reporter observations, native logs, all SQL receipts, signed release
manifests, controllers, and synthetic fixtures. Its SHA-256 is
`227118d7f8a841a159c1b57d467cd676a31ac5895c13fb3ed3526d10742d24c5`.
Public release binaries and GitHub-backed source snapshots are omitted; their
identities are recorded separately. Current-main native compilation and the
three native regression bodies remain unverified.

## What the source review establishes

The supervisor now invokes `mail_readiness()` outside the one-time startup
notification branch. The shared cache permits one queued or running readiness
job per HTTP state, waits two seconds after an attempt completes before
admitting another, and rejects samples older than six seconds. Samples are
timestamped before the work, and publication is guarded by the serving-pool
generation. A fresh failed sample still produces `database_probe_failed`.
The handler does not make the HTTP request wait for a database checkout.

These findings come from the [supervisor and readiness implementation at the
reviewed commit](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/6eb6264d5d6becc2a7609d795c7209c88aac9d55/crates/mcp-agent-mail-server/src/lib.rs).
They support the design of the fix; they are not native execution results.

The one-second probe budget bounds asynchronous checkout and is checked after
synchronous schema/query work. It is not a preemptive wall-clock bound on an
arbitrarily stalled synchronous engine or filesystem call. The independent
runner therefore includes shutdown with a database failure still active.

The native `health_readiness_supervisor_refreshes_without_client_polls` test
starts the real supervisor, waits eight seconds, checks its first readiness
response, retains the only serving-pool connection, requires a failed probe,
checks independent liveness, releases the connection, and checks recovery
without readiness polling. It also joins the supervisor and runtime. The
black-box runner adds repeated idle intervals, concurrent first requests,
actual binary arguments and environment, OS signals, and observed resource
and failed-probe cadence bounds.

## Plan and Beads alignment

The review included `AGENTS.md`, `TODO.md`, the original port plan, relevant
sections of `docs/planning/BRIDGE_PLAN_2026-09-01.md`, the verification coverage
ledger, and relevant records in `.beads/issues.jsonl`. The suite-level
`/data/projects/AGENTS.md` was not present in either executor.

Relevant still-open work includes the slow/false-fail health endpoint record
`br-am-health-endpoint-slow-false-fail-45e0e`, worker ownership
`br-kp1in.90`, resource/liveness instrumentation `br-kp1in.29`, and the
six-hour daemon heap acceptance in `br-kp1in.44`. This short qualification does
not close those broader requirements. The current TODO reports compiler
checks on a separately named c567-based candidate while preserving zero
native assertions; that does not qualify the exact main commit reviewed here.

## Isolation and provenance

The runner accepts a binary path, not an existing server URL or mailbox path.
Every invocation creates and retains a new private run directory. Each server
lifetime has its own database, archive, working directory, project roots,
temporary directory, and XDG configuration/data/cache/state/runtime paths.
A private `mcp-agent-mail/config.env` stops fallback to ambient configuration.
The child environment is an allowlist; `HOME` is neither changed nor inherited.

It binds only `127.0.0.1` on a newly chosen port and verifies through `/proc`
that the supplied process owns that listener before sending HTTP requests.
The launch uses `serve-http --no-tui --no-auth`, never setup, takeover, a shared
mailbox, or an existing daemon. The database URL and storage root are explicit.
The pool has one connection and zero overflow. Periodic database maintenance
and the integrity guard are disabled to keep them from repairing an intentional
schema fault. Readiness timing and the supervisor remain unchanged.

The report records the executable SHA-256, version output, harness SHA-256,
configuration, all first responses, command receipts, and process measurements.
It hashes the binary again after the run. `--source-sha` is explicitly recorded
as a caller assertion; a build receipt is needed to bind an executable to that
commit. A supplied old release is never relabelled as a current-main build.

## Qualification sequence

1. Establish an initially ready server and a successful, uniquely named
   `ensure_project` operation. Warm the requested connection concurrency before
   taking the settled resource baseline.
2. Reproduce the reporter's exact sequence three times by default: a successful
   `/health`, eight seconds without any external HTTP/MCP request, then one
   `/health`. Preserve the first response even if a later request succeeds.
3. Wait another eight seconds with no requests, then concurrently issue 16
   readiness requests across `/health` and `/health/readiness`. Inspect every
   response, not only the first success.
4. Observe a 30-second no-request idle soak. The independent `/proc` monitor
   does not call health, metrics, MCP, or database endpoints.
5. Establish a real, reversible named-table outage through the supplied binary's
   own FrankenSQLite CLI. Prove the table is missing through another CLI query
   and a fresh MCP database operation, then leave health idle for eight seconds.
   Require the first response to be 503 with `database_probe_failed`, all mail
   capabilities false, and both liveness routes still 200/alive. Repeat the
   concurrent readiness burst while the fault is active.
6. Restore the table and original project rows/schema object identities. Wait
   eight seconds without readiness polling. Require the first response to be
   ready and a fresh MCP project operation to succeed.
7. Challenge stale success separately: confirm readiness, snapshot the synthetic
   schema, SIGSTOP the owned process, confirm it is stopped, install the same
   independently witnessed schema fault, wait eight seconds, and SIGCONT.
   The first readiness response must fail closed with stale or failed-probe
   reason. Record which branch actually won the scheduling race. Restore and
   verify recovery again. Always resume a partially suspended process before
   restoration or shutdown.
8. Require a live, same-start-identity process immediately before the shutdown
   signal and a successful SIGTERM syscall. An earlier exit, including exit 0,
   cannot satisfy this check. Require listener closure, no surviving process-group
   members, and no database/archive mutation during a one-second quiet window.
   Repeat shutdown in a separate fresh lifetime while the database fault is
   still active. Forced SIGKILL cleanup is a failed clean-shutdown check.

## Why the database fault is meaningful

The runner invokes the same supplied binary:

```bash
am tooling db-exec --db "$synthetic_db" \
  --sql 'ALTER TABLE projects RENAME TO gh407_saved_projects' --json
```

It first proves the target name is unused and saves the original rows and
schema object identities. After the mutation, `db-query` must fail for
`projects`, the renamed table must contain the original rows, and a new-key
MCP `ensure_project` must produce an attributed database/schema error. Auth,
invalid parameters, timeouts, or a cached success do not establish the fault.

Restoration uses the reverse `ALTER TABLE`, including in failure cleanup. It
does not delete a table or replace the database. Project rows and schema object
identities must match the snapshot. This is a logical restoration check; it
does not claim byte-identical database/WAL files or a complete DDL equivalence
proof. Every fixture and receipt is retained for inspection.

The native CLI helpers use the same `DbConn`/FrankenSQLite driver as serving
operations. The readiness schema requires `projects`, and new project keys
avoid the scoped project cache. See the reviewed [CLI
tooling](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/6eb6264d5d6becc2a7609d795c7209c88aac9d55/crates/mcp-agent-mail-cli/src/lib.rs)
and [database
queries](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/6eb6264d5d6becc2a7609d795c7209c88aac9d55/crates/mcp-agent-mail-db/src/queries.rs).

This fault is source-grounded but still needs execution against native Agent
Mail. A refusal, unsupported ALTER operation, ineffective mutation, or failure
to establish the MCP control yields INCOMPLETE, not a qualification pass. No
alternate engine or owner-guard bypass is substituted.

The runner deliberately avoids assuming that a system-SQLite write lock blocks
FrankenSQLite reads, or that chmod/rename invalidates existing database handles.
It also avoids replacing an activity-lock inode. The full native server already
holds a shared activity lock throughout its lifetime, so an external exclusive
flock cannot simply reproduce the native fixture's sole-pool exhaustion.

## Bounds and verdicts

| Measurement | Default qualification bound |
| --- | --- |
| HTTP response | 5 seconds per request, including timeout failures |
| CLI helper | 20 seconds, then TERM/KILL and at most 4 seconds for terminal drain |
| Initial healthy baseline | 90 seconds; process exit aborts it early |
| Clean server shutdown | 10-second TERM grace; SIGKILL escalation is recorded as failure |
| Settled descriptors | Baseline plus at most 32 |
| Settled threads | Baseline plus at most 16 |
| Settled RSS | Baseline plus at most 64 MiB |
| Average idle and failed-probe CPU | At most 0.25 of one CPU core |
| Failed probe completions | At least two; no more than floor(observed seconds / 2) + 2 |

Every failed native readiness probe emits `serving-pool readiness probe failed`.
The runner counts that message over the separately measured outage window.
Missing/truncated logs do not turn into a zero-work pass. One-second raw
resource samples are retained; the resource assertions compare settled
checkpoints, not every transient peak during the concurrent burst.

Exit codes are 0 for PASS, 1 for FAIL, and 2 for INCOMPLETE or invalid invocation.
FAIL takes precedence over INCOMPLETE. A missing prerequisite is kept separate
from demonstrated bad behavior. The runner does not retry a failed first
readiness response into a pass. Interrupted runs retain an incomplete verdict;
cleanup still resumes and stops the owned processes.

These are bounded smoke observations, not a proof against every future leak,
an exact count of internal single-flight jobs, or a replacement for the
six-hour mixed-load acceptance. The one-second post-exit observation does not
prove an in-process lifecycle barrier; that requires the existing native tests.

## Run the independent harness

```bash
node scripts/qualify_health_idle.mjs \
  --binary /absolute/path/to/current-build/am \
  --source-sha REPLACE_WITH_BUILD_RECEIPT_COMMIT \
  --expected-sha256 REPLACE_WITH_BUILD_RECEIPT_SHA256 \
  --output-parent /absolute/private/qualification-output
```

The output directory need not exist. Each run creates a unique child; it does
not overwrite a prior run or clean up retained synthetic data. The final stdout
names `report.json`. The two session directories contain launch settings,
binary versions, server logs, one-second resource samples, and every SQL helper
receipt. User-configurable idle and soak lengths cannot be shortened below
their qualification minima through the CLI.

## Compile and qualify an exact native build

The automated entry point is `scripts/ci/qualify_issue407.py`. On an authorized
Linux host with Python 3.11 or newer, Node 22 or newer, the source-pinned Rust
toolchain, and sufficient build capacity, run from a clean committed checkout:

```bash
python3 scripts/ci/qualify_issue407.py \
  --expected-source "$(git rev-parse HEAD)" \
  --evidence /absolute/new/gh407-native-evidence
```

The driver records and rechecks the actual committed source bytes and executable
modes, the compiler, lockfile, resolved dependency identities, and Cargo-emitted
executables. Hidden tracked edits, untracked Cargo configuration, compiler-wrapper
overrides, stale binaries, and reports with mismatched identities are refused.
Targets stay outside the evidence upload directory; an existing
`CARGO_TARGET_DIR` can be reused when its ownership and permissions are suitable.

It builds the existing server tests with the original default features, then
runs each of these exact regressions with a separate 180-second runtime bound:

- `tests::health_readiness_supervisor_refreshes_without_client_polls`
- `tests::health_readiness_fails_closed_on_pending_pool_acquisition`
- `tests::health_readiness_invalidates_another_pool_generation`

Each must report exactly one passing, zero failing, zero ignored test. The
legitimate child-and-parent output is accepted; zero selected tests and a
successful compilation alone do not pass. A separately bounded release `am`
build supplies its actual emitted executable and hash to the independent
black-box harness. The production branch can still run after an incomplete test
build when resources permit; overall qualification requires every native test,
the black-box run, and the final source/binary check to pass.

The driver retains raw logs and structured receipts for failure, interruption,
resource refusal, and success. It owns detached descendants through a Linux
subreaper and applies bounded cleanup. It reserves 2 GiB of free space on relevant
filesystems and 1 GiB of available memory, uses one compilation job, and reduces
debug information without changing features or assertions. Its 128 MiB log cap
also applies to a command that exits before the next resource sample.

`.github/workflows/issue407-native.yml` runs on relevant `main` pushes and manual
dispatch. Controller controls run first in a separately labelled job. The native
job checks out the triggering SHA, installs its pinned compiler, and retains
failed and incomplete receipts. A workflow definition or passing controller job
is not native Agent Mail evidence.

The recovered final controller sources passed **24/24 tests in 3.756 seconds**
on November 24, 2026, including real timeout, interruption, and detached-child
cleanup controls. The unchanged JavaScript model suite also passed **28/28**.
Python syntax, workflow YAML structure/action pins, and embedded shell syntax
checks passed. These results supersede the earlier unexecuted-controller state;
they still execute zero Agent Mail native regression bodies.

```bash
python3 -m unittest discover -s tests/ci -p test_qualify_issue407.py -v
node --test tests/test_qualify_health_idle.mjs tests/test_qualify_health_shutdown.mjs
```

For a compiler provisioned without rustup, set PATH and RUSTC to the verified
source-pinned tools and add
`--toolchain-manifest /absolute/channel-rust-nightly.toml`. Retain authenticated
distribution and component-checksum receipts: the driver checks the supplied
manifest's date and compiler identities but does not authenticate that manifest.
Cargo's normal cache integrity remains a trusted build input; the driver checks
resolved identities rather than hashing every cached transitive source byte.

### Recorded native build limitation

An earlier attempt provisioned the exact pinned compiler and fetched all locked
dependencies successfully in 160.233 seconds with `Cargo.lock` unchanged. Its
default-feature server test build on clean source `bc407d6b` stopped at the
declared available-memory reserve after **256.764 seconds**. It had emitted 226
dependency artifacts, with no compiler diagnostic identifying a source defect,
but had not completed Asupersync or produced the server test executable. Peak
owned build-process-group RSS was 4,552,830,976 bytes; available host memory fell
to 969,211,904 bytes. Cargo exited on SIGTERM, without forced SIGKILL. Native
regression bodies executed: **zero**.

A lower-memory diagnostic was not launched: shared workspace free space then
fell below 400 MB while the measured task-owned target/cache allocations stayed
unchanged, and the executor disconnected. Original receipts were saved before
the outage but could not be exported afterward. These observed results are a
resource-limited incomplete qualification, not a native pass. A separate pinned
format check also reported existing differences in
`crates/mcp-agent-mail-server/src/retention/pending/journal_scan.rs`; no
workspace-wide formatting pass is claimed.

### Individual build and qualification commands

Use an authorized build host with the repository's pinned compiler and enough
disk. Keep `Cargo.lock`, dependency revisions, default features, and native
assertions unchanged. If the host uses RCH, route compilation through its
ordinary `rch exec -- cargo ...` admission; do not bypass a refused resource
budget or substitute a different revision. The commands below show the direct
form for an authorized local build host.

```bash
set -euo pipefail

# Run in a checkout at the exact source you intend to qualify.
qualification_source_sha="$(git rev-parse HEAD)"
qualification_lock_sha="$(sha256sum Cargo.lock | cut -d ' ' -f 1)"
git diff --exit-code -- Cargo.toml Cargo.lock rust-toolchain.toml crates

cargo +nightly-2026-08-31 fetch --locked

# Separate compilation from test-body runtime. Retain the build logs.
timeout --signal=TERM --kill-after=10s 3600s \
  cargo +nightly-2026-08-31 test --locked \
  -p mcp-agent-mail-server --lib --no-run

timeout --signal=TERM --kill-after=10s 180s \
  cargo +nightly-2026-08-31 test --locked \
  -p mcp-agent-mail-server --lib \
  tests::health_readiness_supervisor_refreshes_without_client_polls \
  -- --exact --nocapture

timeout --signal=TERM --kill-after=10s 180s \
  cargo +nightly-2026-08-31 test --locked \
  -p mcp-agent-mail-server --lib \
  tests::health_readiness_fails_closed_on_pending_pool_acquisition \
  -- --exact --nocapture

timeout --signal=TERM --kill-after=10s 180s \
  cargo +nightly-2026-08-31 test --locked \
  -p mcp-agent-mail-server --lib \
  tests::health_readiness_invalidates_another_pool_generation \
  -- --exact --nocapture

timeout --signal=TERM --kill-after=10s 3600s \
  cargo +nightly-2026-08-31 build --locked --release \
  -p mcp-agent-mail-cli --bin am

test "$(sha256sum Cargo.lock | cut -d ' ' -f 1)" = "$qualification_lock_sha"
git diff --exit-code -- Cargo.toml Cargo.lock rust-toolchain.toml crates

# Use the actual emitted binary path if CARGO_TARGET_DIR is configured.
qualification_binary="$(pwd)/target/release/am"
qualification_binary_sha="$(sha256sum "$qualification_binary" | cut -d ' ' -f 1)"
node scripts/qualify_health_idle.mjs \
  --binary "$qualification_binary" \
  --source-sha "$qualification_source_sha" \
  --expected-sha256 "$qualification_binary_sha" \
  --output-parent "$(pwd)/qualification-output"
```

Require a nonzero, exact native test count for each filtered invocation. A
successful `--no-run`, Cargo check, zero selected tests, or a surrogate pass is
not an executed native regression. Preserve failed first runs and timeouts.
Do not silently increase deadlines until a run happens to pass.

The inspected exact pins are:

| Component | Selection |
| --- | --- |
| Toolchain | `nightly-2026-08-31` |
| Asupersync 0.5.0 | `2946438a5d2bd164c9aee73a5ea079afe0c13907` |
| SQLModel core/pool/FrankenSQLite 0.5.1 | `ca959f899b98fbea7af19e942e687dae9b71f258` |
| FrankenSQLite core 0.4.14 | `e7ffeff4eb1eda0a4d62d00bbe778d6103058e7f` |
| FrankenSearch 0.6.1 | `616c9a7a6bdada97d81f760808996911ba3be294` |
| Direct Tantivy 0.27.0 | `1f32c1a8af0eb9d68bd3f5576caf20941364b657` |

`Cargo.lock` SHA-256 at the reviewed commit is
`98b26497fb52c79582497c745654e6a7433a7a8b9178e5965650c1e37e751938`.
It remains authoritative for all other dependency selections.

For the repository-wide compiler gates on the authorized host, run the
separate production and test graphs without changing features:

```bash
cargo +nightly-2026-08-31 check --locked --workspace --all-targets
cargo +nightly-2026-08-31 check --locked --workspace --lib --bins
cargo +nightly-2026-08-31 clippy --locked --workspace --all-targets -- -D warnings
cargo +nightly-2026-08-31 fmt --check
```

## Reproduce harness validation

The fast model suite exercises the actual orchestrator using virtual time. It
preserves the real two-second refresh/six-second age/eight-second idle contract.

```bash
node --check scripts/qualify_health_idle.mjs
node --test tests/test_qualify_health_idle.mjs tests/test_qualify_health_shutdown.mjs
```

To exercise the real OS/HTTP adapter with the healthy surrogate, use Node 24:

```bash
node scripts/qualify_health_idle.mjs \
  --binary "$(pwd)/tests/fixtures/gh407_agent_mail_surrogate.mjs" \
  --evidence-class harness_surrogate \
  --output-parent "$(pwd)/harness-validation-output"
```

The surrogate supports intentionally broken modes through its exported
`fixtureMain(mode, argv)` function: `idle_refresh_missing`, `false_ready`,
`false_capabilities`, `polling_only_recovery`, `ignore_term`, `blocked_fault`,
`ineffective_fault`, `auth_only_fault`, `malformed_health`, and `restore_failed`.
An executable wrapper selecting one of these modes can be supplied as the
binary. The full validation receipts preserve the wrappers and exact source
hashes. All such results are harness validation only.

## Remaining native uncertainties

For current main, native execution must still confirm the exact pinned
compiler/dependency graph, all three existing readiness regressions, and the
full black-box qualification matrix, including the suspended stale-success
challenge, outage liveness, failed-probe cadence, and resource behavior. The
historical control above confirms the schema-fault adapter and clean SIGTERM
only for v0.3.41; it does not establish the current fix's behavior. A successful
native run would qualify only its recorded source, binary, feature graph,
isolated workload, and observation window.

No production correction was indicated by the independent source review.
The existing supervisor fix remains the implementation under qualification.
