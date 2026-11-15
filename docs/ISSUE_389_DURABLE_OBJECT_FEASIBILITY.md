# GH #389: one Durable Object per mailbox — feasibility evidence

Status: **prototype feasible; production and 50-RPS acceptance not established**.
Scope: revision in [issue #389](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/389#issuecomment-6882076148), tested against repository revision `b4ea4daa35dd` (2026-11-14).

## Architecture decision under test

Use one SQLite-backed Durable Object **per mailbox**, not per project. Projects remain
rows inside a shared database, preserving transactional cross-project delivery,
product-bus state, unified inbox, global search, and event ordering. A separate
remote deployment target must leave the native server as the default.

This is **not** a completed port: the prototype used an isolated JavaScript Durable
Object adapter with SQL extracted from Agent Mail, not the Rust MCP server.

## Local Durable Object correctness observations

16/16 focused probes passed in the local workerd environment:

- 30 simultaneous duplicate sends with one idempotency key: one persisted
  message/event and 29 replays. A changed payload with that key was rejected.
- Replay of a committed send succeeded even after the contact policy changed;
  a *new* send was denied.
- Failure injected after message insertion, and after recipient/event insertion,
  rolled back partial delivery.
- Per-recipient acknowledgements remained independent; repeated acknowledgements
  preserved their timestamp.
- 30 exclusive reservations on the same exact path: one grant, 29 conflicts.
- Graceful local runtime restart preserved messages, acknowledgements, retry
  results and event cursor.

These are subset tests. They do **not** cover full glob conflict behavior,
keyed ACK replay, complete migrations, eviction, sudden process death, R2,
alarms, auth, or end-to-end MCP conformance.

**Critical concurrency invariant:** a Durable Object can interleave requests
across an `await`. In a negative control, ten read/await/write increments
produced only one net increment. Put reads, decisions, writes, events and
idempotency-result recording in the same **synchronous SQLite transaction**.
Do not perform network I/O inside it. `transactionSync()` is the supported
Durable Object SQLite mechanism; issuing `BEGIN`/`SAVEPOINT` directly is not.

## Load experiment (local workerd, not deployed Cloudflare)

30 clients; five projects; 10,000 seeded messages (~1 KiB body);
50 scheduled requests/second for 60 seconds; mixed sends, inbox reads,
search, exact-path reservations, and acknowledgements.

| Metric | Observed |
| --- | ---: |
| Scheduled | 3,000 |
| Dispatched/completed | 2,993 |
| Server errors among dispatched | 0 |
| Client-side drops at 30-in-flight cap | 7 |
| Late arrival attempts | 104 |
| Completed throughput | 49.884 requests/s |
| Median latency including schedule delay | 6.541 ms |
| p95 | 174.312 ms |
| p99 | 412.899 ms |
| Maximum completed-request latency | 725.904 ms |

All 1,198 successful cross-project sends had corresponding message, recipient,
FTS, event and retry records. Final corpus: 11,198 messages, 145 acknowledgements,
300 reservations. Post-load read-only integrity checks passed.

**Do not mark the proposed 50-RPS gate passed.** Seven scheduled requests
never dispatched; the generator exhibited jitter and catch-up bursts. An
earlier completed run dispatched 2,948 requests and dropped 52; another
attempt stalled without a complete summary. A separate initial verification
query performed quadratic work and timed out; its instrumentation was
corrected for the later run. The stalled attempt has not been explained.
The reported latency percentiles exclude dropped requests.

## Verified Rust/WASM obstacle

`cargo +nightly-2026-08-31 check --locked -p mcp-agent-mail-core --lib --target wasm32-unknown-unknown --no-default-features`
failed in the `mcp-agent-mail-core -> rustix 1.1.5 -> errno 0.3.14`
dependency path because errno does not support target OS `unknown`.
Thus `--no-default-features` is **not** a portable-core boundary today.

A compile-only control containing the **unchanged**
`crates/mcp-agent-mail-core/src/pattern_overlap.rs` with
`globset 0.4.20` compiled for `wasm32-unknown-unknown`.
This is evidence that **some decision logic can be extracted**, not that
the whole core already works on Workers.

Also resolve the dependency policy before choosing workers-rs: the examined
workers-rs 0.9.0 manifest declares a Tokio dependency even with default
features off. That does not by itself imply a threaded runtime, but it
conflicts with a blanket no-Tokio dependency policy.

## Behavioral incompatibilities not hidden by a storage trait

1. **Search:** the legacy FTS tokenizer and native Tantivy tokenizer can
   disagree on *which documents match* (not just ranking). Use differential
   conformance fixtures and specify intended behavior.
2. **Path reservations:** the production pattern matcher chooses case
   sensitivity with the server's `target_os`. A remote WASM server must
   receive an explicit workspace filesystem policy; the Workers OS is not
   a proxy for the client's Mac/Windows/Linux filesystem.
3. **Native client:** robot/TUI/guard operations that open local SQLite or
   inspect tmux/Git must go through coarse `MailboxClient` API calls or
   remain host-side with explicit evidence.
4. **SQL API:** local probes accepted 100 bound parameters but rejected
   101, and accepted LIKE patterns up to 50 bytes but rejected 51.
   Batch SQL and bound variable usage accordingly.
5. **Integer transport:** JavaScript Number loses precision for values
   above 2^53; use exact strings for wide IDs/counters at the boundary.
6. **Recovery:** local graceful restart is not cloud eviction, crash,
   replication, or point-in-time-recovery testing. R2 side effects are
   not part of the mailbox SQL transaction.

## Go/no-go sequence

1. Extract pure decision types and rules into a platform-neutral crate.
   Add a real `wasm32-unknown-unknown` build gate on **that crate**, not
   an `--all-features` build of the native server.
2. Introduce `MailboxStore` in front of a single mailbox operation.
   Commit the idempotency result in the same transaction as delivery.
3. Implement native and DO adapters and run common conformance cases
   for send, retry, rollback, inbox/events, ACK and reservation contention.
4. Add a coarse `MailboxClient` call to native robot/TUI clients before
   multiplying HTTP round trips.
5. Replay a **calibrated, drop-free** 30-client / 50-RPS workload against
   the actual Rust-backed DO and deployed Cloudflare; include queueing,
   network latency, rejections, alarm effects, and all missed arrivals
   in the accounting. Investigate the prior stall.

**Decision:** pursue an isolated prototype behind an opt-in deployment
target. Do not declare native-runtime parity, end-to-end Rust/WASM support,
or 50-RPS production capacity proven by these experiments.
