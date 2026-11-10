# Large-mailbox qualification (GH382)

This work addresses the startup, unified-inbox, and lexical-search paths reported
in [issue #382](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/382).
The reporter measured commit `e7ffb20f` on a production-mailbox copy with 242
projects, 12,236 agents, and 203,498 messages, under a two-core CPU quota and a
4 GiB memory limit. Those measurements are the reporter's baseline, not timings
from the synthetic fixtures introduced here.

## Evidence status

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
