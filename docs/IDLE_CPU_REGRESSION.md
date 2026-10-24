# GH#354: idle worker CPU regression and attribution

## Evidence boundaries

The report concerns `am serve-http` 0.3.38 musl (also observed with 0.3.36),
one `fsqlite-worker` at 47–88% CPU, and a roughly 9,362-message store. No sampled
userspace stack or reproducer database is attached. Thread names alone cannot
distinguish executing SQL, connection shutdown, or a mailbox receive loop.

The October 13 control receipt is a **write-stall** experiment, not an idle CPU
baseline:
`docs/evidence/release_smoke/2026-10-13_a4ec56bc-release_vs_v0.3.38_write-stall_round1_control.json`.
Its parked-worker samples are useful comparisons, not proof that the October 14
report was resolved. Likewise, the reservation recovery work for #353 is not
acceptance evidence for #354.

Both the v0.3.38 manifest and the inspected main manifest pin FrankenSQLite
0.4.10, SQLModel 0.5.0, and Asupersync 0.5.0. Reading a newer dependency's main
branch is not a substitute for profiling the packaged binary and its exact
locked sources. Do not claim that upgrading the dependency fixes this report
without a demonstrated version difference and a before/after measurement.

The inspected integrity-guard loop waits after every cycle; database maintenance
is subordinate to successful integrity verdicts. `DB_MAINTENANCE_ENABLED=false`
is an **isolation arm**, not a verified remedy. Keep the guard enabled, and do
not alter WAL/sync settings, abandon connection shutdown, discard recovery debt,
or turn off indexing merely to improve an idle CPU number.

## Probe

`scripts/idle_cpu_probe.py` uses only Python's standard library and Linux `/proc`.
It attaches to a PID; it does not launch/stop the server, change configuration,
open its database, or send any HTTP requests during warmup or measurement.
The optional workload runs **after** that window. Absence of traffic from other
clients is not inferred: use an isolated fixture and endpoint.

```bash
python3 -m unittest discover -s tests -p test_idle_cpu_probe.py -v

# SERVER_PID must identify the actual am process, not a shell or PTY wrapper.
# Choose a sufficiently long warmup for startup indexing/recovery to finish.
python3 scripts/idle_cpu_probe.py \
  --pid "$SERVER_PID" --warmup 60 --seconds 300 --interval 1 \
  --max-idle-cpu-percent 10 --output idle-baseline.json

# Full gate: same idle window, then a correctness-checked normal workload.
# Use ONLY a disposable fixture: these cases create an agent and send a message.
HTTP_BEARER_TOKEN="$FIXTURE_TOKEN" python3 scripts/idle_cpu_probe.py \
  --pid "$SERVER_PID" --warmup 60 --seconds 300 --interval 1 \
  --max-idle-cpu-percent 10 --max-latency-ms 5000 \
  --url http://127.0.0.1:18765/api/ \
  --workload tests/fixtures/idle_cpu_workload.json --output idle-and-workload.json
```

The workload is a JSON array. Every case has a JSON-RPC `request` and nonempty
`expect_contains` strings checked against its result. The example creates an
isolated project/agent, sends a token-bearing message, then fetches and searches
for that token. `{{RUN_TOKEN}}` is replaced in requests and assertions by a fresh
per-run token, so stale messages cannot satisfy a new send/search check. The
report records this non-secret token; use `--run-token` to reuse it for read-only
checks after restart. Change the example project key for your fixture. A fast HTTP 200
with an RPC error, tool error, mismatched request ID, or missing expected data
fails. The bearer token is read from the named environment variable; no request
or response bodies are included in reports. Redirects and proxies are disabled,
and workload URLs must be loopback HTTP. Use a surrounding process timeout in
an automated runner as well: the request timeout is a socket-operation timeout,
not a guarantee against a server that indefinitely trickles response bytes.

Exit codes: **0** all requested checks passed; **1** CPU or workload regression;
**2** inconclusive (for example, the process disappeared, its PID was reused,
is a zombie, or `/proc` could not be read). A CPU-only run explicitly reports
`workload_status: not_requested`; it is not full workload acceptance.

CPU is `(delta user ticks + delta system ticks) / CLK_TCK / elapsed monotonic
seconds`. **100% is one logical CPU**, not the whole host. Process CPU is the
authoritative gate and includes work by threads that exit between samples.
Per-thread attribution is deliberately a lower bound: it omits unsampled
lifetime CPU of newly discovered threads and cannot recover final ticks of
threads that exit between samples. Threads are keyed by TID plus start time,
not by their shared `fsqlite-worker` name. Missing workers are reported as
unknown (`null`), not as zero CPU. Each interval records the hottest sampled
threads and their wait channels. A `futex_do_wait` observation is instantaneous;
it does not imply that the thread slept throughout that interval.

## Controlled acceptance matrix

Use separate, offline-consistent copies of the same database, archive, and
Tantivy index for each arm. Do not copy a live database without a supported
consistent backup procedure. Record binary SHA-256, `am --version`, target
(musl/glibc), commit, environment, CPU quota, filesystem, message count, and
recovery/indexing backlog. Never point the workload at an operator's live store.

Run the unmodified and candidate binaries in alternating order on the same
otherwise quiet host. Cover an empty initialized store and a populated store of
approximately 9,362 messages. Include the reported launch mode, a headless arm,
a restart after initial catch-up, and a run long enough to cross normal
maintenance intervals. Preserve all default safety mechanisms in the principal
comparison. Separate isolation arms may disable database maintenance or the TUI
poller, one at a time, but are not the acceptance configuration.

After idle measurement, run the send/fetch/search workload. Repeat the read and
search cases after a clean server restart to check persistence and search
visibility. A deliberate missing archive/lease projection should still be
repaired under the existing recovery tests. A low CPU number while recovery
stalls or messages become unsearchable is a failure, not an improvement.

When a worker is hot, use the reported TID for a bounded userspace stack sample
on the fixture (for example, `perf record -t TID -g -- sleep 15`, with the
matching symbols). Correlate SQL/query metrics, connection creation/retirement,
recovery progress, and index activity with those same intervals. Distinguish:

- commands repeatedly submitted by an application poller;
- one slow or stuck SQL/maintenance operation;
- connection cleanup or dependency runtime work;
- an actually empty command mailbox that does not park.

Choose the correction only after that distinction. An application scheduling
fix must retain event wakeups, a bounded external-change fallback, retry
backoff, and shutdown ownership. A dependency fix must use the exact released
sources and prove both idle parking and subsequent command responsiveness.

## Validation in this change

The probe's standard-library tests exercise interval arithmetic, parenthesized
thread names, TID/PID reuse, thread churn, missing observations, missing
processes, zombie rejection, unique workload tokens, invalid durations,
HTTP correctness/error/latency gates, token-safe
redirect rejection, real sleeping/spinning processes, and a real loopback
post-idle workload. These are **probe calibration**, not an Agent Mail
performance result. No packaged Agent Mail idle before/after measurement or
Rust test execution has been obtained in the authoring environment. Issue #354
must remain open until the real acceptance matrix identifies and verifies a
production correction.
