# GH274: integrated cold overview collection

The first sections below record the initial collector's historical evidence.
For the later bounded, indexed recipient phase, see the final section. The
measurements cover different query models and must not be compounded.

Implementation commit: `32b40410dcb248129c4e7fa74a6a3c5544fac5f6`.
Evidence recorded on 2026-09-18. This is SQL-work evidence, not a native CLI
latency claim.

## Production change

`robot::handle_robot` now routes overview and overview --counts through the
live collector in `src/robot/overview.rs`. Other commands use the preserved
implementation in `robot_commands.rs` (original Git blob
`f8ca6f74bb4621882b71ef8148a156752e5e9cb5`).

The cold command no longer computes the whole-mailbox generation fingerprint
before consulting a process-local cache. Five narrow data scans replace the
recipient/message JOIN and GROUP BY and the orphan-project discovery joins.
Additional reads probe the reservation schema and read the release ledger;
"five scans" does not mean five total SQL statements. A Rust hash map matches
recipient rows to message metadata, and release-ledger membership is checked
in a fresh hash set. The existing guarded database opener is retained.

A nested read savepoint covers collection and is released on success or query
failure. Counters are recalculated on every call, including expiry/overdue
changes without writes and read/ack edits that do not advance a maximum
observed timestamp. The counts/full JSON and TOON output use the original
public envelope and formatting functions. Native tests pin legacy/ledger
schema variants, null-versus-zero semantics, strict boundaries, orphan
visibility, read-only transactions, and output shape.

## Executed diagnostic

Run from a checkout containing the production change:

```sh
python3 scripts/verify_gh274_overview.py
```

Eight Python test methods passed, including 100 seeded differential fixtures,
input-row scaling, actual two-connection canonical SQLite WAL snapshot
visibility, outer-transaction preservation, and error cleanup. The four core
SQL projections are extracted from the Rust source; application-side matching
is independently implemented in Python. These are not executions of Rust.

Canonical SQLite 3.46.1, `set_progress_handler` with instruction interval 1:

| Projects | Messages | Recipients | Previous cold SQL VM instructions | New SQL VM instructions | Recipient lookups |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 10 | 30 | 1,883 | 550 | 20 |
| 10 | 100 | 300 | 17,075 | 5,113 | 200 |
| 50 | 500 | 1,500 | 84,595 | 25,393 | 1,000 |
| 50 | 24,000 | 72,000 | 3,985,595 | 1,200,393 | 48,000 |

Each scaling fixture has one expired reservation per message. The large case
therefore has 24,000 expired reservations. Old/new results compare exactly.
The large case executes about 69.9% fewer canonical SQL VM instructions.

The previous-cold model includes the former generation query plus the old
live collector's query shapes. The new model uses the narrow projections and
release/reservation reads. VM counts exclude application hash work, SQL
schema probes, savepoint-control overhead, database opening, and CLI startup.
The fixture uses the ledger-only schema and does not reproduce every
production index. Neither these numbers nor the Python test run time are a
FrankenSQLite/Rust speedup measurement; native planner and allocation costs
can differ materially.

## Native validation still required

Eight Rust regressions and one ignored native DbConn benchmark are added. The
read-only workflow also targets the existing separate-process CLI mutation
test in `integration_runs`. Correct commands are:

```sh
cargo test -p mcp-agent-mail-cli --lib robot::overview::tests -- --nocapture
cargo test -p mcp-agent-mail-cli --test integration_runs robot_overview_cold_processes -- --nocapture
cargo test --release -p mcp-agent-mail-cli --lib benchmark_linear_overview_against_current_main -- --ignored --nocapture
```

The native benchmark compares old/new live collectors with alternating run
order, not CLI startup. Its speedup assertion is opt-in. It does not include
the removed cache-generation cost. A smaller fixture can be selected with
`AM_OVERVIEW_BENCH_MESSAGES_PER_PROJECT=48` (default 480).

Rust compilation, rustfmt, Clippy, native tests, and native timing were not run
in the editing environment: no Rust toolchain was available. GitHub reported
no workflow runs for the implementation commit when checked. Do not close
GH274 on a claim of measured native latency from this diagnostic.

## Scope and tradeoffs

This is not a persistent or per-project incremental cache. A cold overview
still reads narrow metadata across the mailbox. Rust memory now includes the
message map and returned recipient rows; the collector reduces engine query
work by doing matching in application memory, not by making large mailboxes
constant-cost. No body text or reservation path payload is fetched. The
previously fixed reservation anti-join remains absent.

## Indexed recipient counts: 702f1b3e (2026-09-18)

This follow-up builds on the bounded keyset collector already on `a20df51d`.
The remaining recipient predicate was `read_ts IS NULL OR ack_ts IS NULL`.
Read messages that never required acknowledgement still have NULL `ack_ts`,
so every overview kept returning those historical recipients and fetching
message metadata that could not contribute to any counter.

The live path now uses existing full indexes with prefixes `(ack_required,
id)` and `(ack_ts, message_id)`. After a strategy sample of at most 256 pending
recipient rows, the sparse path collects unread recipients and counts
read-but-unacknowledged recipients only for overdue, ack-required messages.
The latter query groups one table for at most 256 message IDs at a time; it
has no JOIN and returns at most 256 groups even with large recipient fanout.
Unread recipients contribute overdue counts in the first pass and are
excluded from the second, so they cannot be counted twice.

Missing/unsuitable indexes use the unchanged bounded scanner. An unread-heavy
sample also selects that scanner, avoiding an additional acknowledgement
message walk on an all-unread mailbox. The sample affects only the choice of
exact algorithm, not which records count. It is a heuristic, not a guarantee
of the cheapest plan for every distribution. No read path creates indexes.
All phases remain under the same existing read savepoint.

Reproduce the recipient-phase diagnostic with:

```sh
python3 scripts/verify_gh274_sparse_recipients.py
```

Eight Python test methods passed, including 200 deterministic randomized
fixtures with assertions that both strategy branches execute. Other cases
cover fanout, signed keys, NULL/zero timestamps, time transitions, below-max
read/ack mutations, index fallback, error cleanup, caller transactions, and
a real two-connection canonical SQLite WAL mutation between the two passes.
The script extracts projections and predicates from the committed Rust
modules and records their SHA-256 hashes. Its control flow is independently
implemented in Python; it does not execute the Rust code.

Measured with canonical SQLite 3.46.1, one progress callback per VM instruction.
The fixture contains 33 project IDs, 24,000 read non-ack-required historical
messages and 33 overdue ack-required messages with three recipients each:
24,033 messages and 24,099 recipients total.

| Recipient-phase measurement | Previous bounded scanner | Indexed strategy |
| --- | ---: | ---: |
| Data statements | 190 | 5 |
| Message metadata rows returned | 24,033 | 33 |
| All data rows returned, including sample and grouped counts | 48,099 | 388 |
| Canonical SQL VM instructions | 820,120 | 79,303 |

The VM reduction is approximately 90.3%. Data-statement and returned-row
counts exclude schema/savepoint probes; the VM totals include those probes
within the diagnostic phase. All figures exclude project inventory,
reservations, opening the database, CLI startup and application-side work.
They are not a FrankenSQLite latency or native CLI speedup measurement.

An adverse comparison changes every message to ack-required and every
recipient to unread/unacknowledged. The strategy selects the old scanner:
190 versus 191 data statements, and 868,451 versus 872,327 canonical VM
instructions (approximately 0.45% added work for the selection check).

Nine native regressions and an ignored native recipient-phase benchmark are
included in `overview/sparse_recipients.rs`. The benchmark alternates run
order and compares the actual old/new Rust helpers on one indexed fixture.

```sh
cargo test -p mcp-agent-mail-cli --lib robot::overview -- --nocapture
cargo test -p mcp-agent-mail-cli --test integration_runs robot_overview_cold_processes -- --nocapture
cargo test --release -p mcp-agent-mail-cli --lib benchmark_sparse_recipient_counts_against_pending_row_scan -- --ignored --nocapture
```

Native compilation, tests, rustfmt, Clippy and native timings remain unrun in
the editing environment, which has no Rust toolchain. The optimization still
requires an engine-side scan to filter unread recipients, can walk historical
ack-required messages, and adds index-metadata probes. Apart from the bounded
strategy sample, it avoids returning read non-ack recipient history or
looking up those messages. It does not implement persistent counters or
cross-process cache reuse. GH274 still needs native/original-corpus latency
validation before a conclusive performance-closure claim.
