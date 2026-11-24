# GH407 independent readiness qualification

This is an independent qualification harness for a supplied Agent Mail binary.
It complements the supervisor change in `c56722595d20182d37b0fced7313b70307940067`
and its real-supervisor regression. It does not alter the supervisor or any
production Rust code.

Reviewed source: `6eb6264d5d6becc2a7609d795c7209c88aac9d55`, rechecked against
`462914b703b93ea0d3d070dccf990e225a0a189a` before publication. The intervening
changes do not alter the supervisor, native readiness tests, or pinned
dependencies. Native Agent Mail compilation and test execution remain
**unverified** by the measurements below. The harness-validation executor had
Node and Python but no Rust toolchain or supplied Agent Mail executable. An
earlier executor disconnected during preflight and produced no retained build
result. This is not evidence that the pinned dependencies are unavailable.

This contribution adds qualification tooling and documentation. It leaves
production Rust, dependency manifests, and Beads status unchanged.

## Qualification files

| File | Purpose |
| --- | --- |
| `scripts/qualify_health_idle.mjs` | Linux black-box runner using Node built-ins; no package installation or native source hooks. |
| `tests/test_qualify_health_idle.mjs` | Adversarial model tests of the runner's exact orchestration and verdict logic. |
| `tests/fixtures/gh407_agent_mail_surrogate.mjs` | Clearly labelled process/HTTP/system-SQLite surrogate for testing the runner's real adapter. |
| `docs/GH407_READINESS_QUALIFICATION.md` | Reproduction, evidence interpretation, source findings, and remaining qualification work. |

The harness requires Linux `/proc`, Node 20 or newer, and `getconf`. The optional
surrogate uses `node:sqlite` and was exercised with Node 24.22.3. It is not
FrankenSQLite, Asupersync, or an Agent Mail build.

## Measured harness validation

The final executable harness source SHA-256 is
`0a267ad29966761cc94c3f7bcad7dd06d067dceea31791459c8217e982e30ce4`.
The process surrogate source SHA-256 is
`4d7284ffbd6cd532b995623d7437a7f173933ef31fc3611a7414433654f2fd60`.
The final integration audit verifies that the files remained identical to the
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
8. Require a clean SIGTERM shutdown, listener closure, no surviving process-group
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
  --source-sha 6eb6264d5d6becc2a7609d795c7209c88aac9d55 \
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
node --test tests/test_qualify_health_idle.mjs
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

Native execution must still confirm the exact pinned compiler/dependency graph,
the current real-supervisor regression, the real pool-exhaustion/concurrency and
generation controls, compatibility of the reversible schema fault with live
FrankenSQLite connections, native SIGTERM behavior during failure, and observed
native resource/cadence bounds. A successful native run would qualify only its
recorded source, binary, feature graph, isolated workload, and observation window.

No production correction was indicated by the independent source review.
The existing supervisor fix remains the implementation under qualification.
