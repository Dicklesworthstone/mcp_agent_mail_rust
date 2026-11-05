# GH354 source-query path evidence

This directory records a complementary native-engine query experiment for
[issue #354](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/354)
and the [November 5 contributor reproduction](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/354#issuecomment-6663146265).
The contributor estimated approximately 40% of a CPU core from the observed
lookup frequency and latency, and reported an isolated query improvement from
100–180 ms to 20 ms. That report did not verify savings inside the running
server. The results below were measured separately on a synthetic mailbox using
the official **FrankenSQLite 0.4.13 CLI**.

During integration, main already contained the reservation narrowing in
[`3455c62`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3455c62c53365ddf51d269973979cf00e40e3ca1)
and message/delivery narrowing in
[`f4c3cdf`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f4c3cdf5339bcc885cfa4ad4209353aba6a399f3).
This follow-up preserves those fixes and adds a shared reservation source helper,
explicit source-column projections, and broader query-cost, byte-limit,
snapshot, and repair-convergence regressions. The measurements identify the
explicit-projection follow-up through its SQL hashes; they do not credit it as
the first implementation of the narrowing fix.

## Observed result and its scope

| Complete source query | Legacy median CPU | Candidate median CPU | Legacy/candidate | Legacy median wall | Candidate median wall |
|---|---:|---:|---:|---:|---:|
| Reservation, ID 4741 | 143.501 ms | 2.692 ms | 53.31× | 146 ms | 3 ms |
| Message, ID 4121 | 276.8685 ms | 9.075 ms | 30.51× | 822 ms | 31.5 ms |

Each successful timing run used one native CLI process/connection, both query
variants, one warm-up per variant, and six measured observations per variant.
Order alternated between legacy first and candidate first. CPU is the CLI
`.timer` user plus system time for the complete source statement. Startup,
canonical reference queries, and diagnostic marker statements are outside those
measurements. The two variants made the same number of source calls. These data
measure lower cost per lookup and do not demonstrate fewer background calls.

All completed warm-up and measured projections matched both native variants
and canonical SQLite, including every CSV column and cell. Reservation output
had one row and 14 columns; message output had four rows and 24 columns, with the
complete 1,280-byte body present once. `results.json` retains every raw measured
user/system/wall sample, warm-ups, totals, query counts, column names, and hashes
of the matching complete CSV projections. It omits repeated body text and the
generated database.

The CLI's documented commands expose no parameter-binding command, so the
experiment renders only the numeric `?1`, `?2`, and `?3` values as decimal
literals. This does not validate Rust facade binding. CSV also cannot
distinguish NULL from empty text or scalar types with identical text
representations. Native Rust typed-row, snapshot, publication, byte-limit and
retention regressions remain required. No native Rust results or running-server
CPU results are claimed by this dataset.

The [native Rust validation record](validation.md) documents three compiler
attempts killed in the unchanged Asupersync dependency and provides the exact
application acceptance commands still to run. No native Rust test or
integration result is implied by the successful CLI probes here.

The full native `EXPLAIN QUERY PLAN` output is retained in the JSON. It labels
both reservation variants `SCAN file_reservations` and both message variants
`SCAN messages`. Those coarse labels do not reveal the executed lookup path and
are not used as proof of indexed execution.

`point-lookup-explain-4741.txt` separately retains the native opcode programs of
the two isolated reservation/release point SELECTs with literal ID 4741. Each
opens cursor 0 with the expected table name in P4, then uses `SeekRowid` without
`Rewind`. This establishes that those isolated literal lookups are indexable.
It does not establish execution of the complete joined query or parameter
binding through the Rust driver.

## Binary and fixture identity

- Release asset: [fsqlite-0.4.13-linux_amd64.tar.gz](https://github.com/Dicklesworthstone/frankensqlite/releases/download/v0.4.13/fsqlite-0.4.13-linux_amd64.tar.gz)
- Archive SHA-256: `c0be5ff014ba34e4173a69b315e5514d5935125bcd6e398d2fcf4d1830d22784`
- Extracted binary SHA-256: `85ca242fb6d84dcdac9cd53759496f50f0753499dbbea679e41e96d5a954014c`
- Version output: `fsqlite 0.4.13`.
- Canonical fixture constructor: Python `sqlite3`, SQLite 3.53.1; the source ID
  is retained in the JSON.
- Main database size at measurement: **17,760,256 bytes**.
- Cardinality: **5 projects, 82 agents, 8,241 messages, 24,723 direct deliveries,
  1,177 project deliveries, 9,482 reservations, and 7,112 release-ledger rows**.
- Every message body is the JSON's `body_unit` repeated 20 times: 1,260 Unicode
  characters and **1,280 UTF-8 bytes**. Other metadata, receipts, release shapes,
  and the generation identity are constructed in `probe.py`.

The fixture uses the repository's base DDL, excluding legacy FTS objects exactly
as `init_schema_sql_base` does. The harness checks that DDL's hash and all four
SQL hashes. The measured reservation candidate is the shared helper's explicit
projection. The hashes identify that SQL independently of incoming alternative
rewrites or a later commit identifier. Message queries retain the literal
`LIMIT 1026` overflow witness.

For reproducibility the extractor preserves the original Python
`ast.literal_eval` behavior: source string continuation indentation becomes
extra SQL whitespace, whereas Rust strips it. Hashes cover this extracted SQL
before numeric substitution and the final semicolon. No SQL predicate,
projection, join, or limit is changed during extraction.

## Reproduce

Use Linux amd64 and a fresh owned scratch directory. The following commands
download and validate the same binary; the Python extraction rejects absolute
paths, parent traversal, links, and special files in the archive.

```bash
mkdir -p /tmp/am-gh354-query-probe
curl --fail --location --proto '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 90 \
  --user-agent 'mcp_agent_mail_rust-agent (+https://github.com/Dicklesworthstone/mcp_agent_mail_rust)' \
  --output /tmp/am-gh354-query-probe/fsqlite-0.4.13-linux_amd64.tar.gz \
  https://github.com/Dicklesworthstone/frankensqlite/releases/download/v0.4.13/fsqlite-0.4.13-linux_amd64.tar.gz
python - <<'PY'
import hashlib, pathlib, tarfile
root = pathlib.Path('/tmp/am-gh354-query-probe')
archive = root / 'fsqlite-0.4.13-linux_amd64.tar.gz'
assert hashlib.sha256(archive.read_bytes()).hexdigest() == 'c0be5ff014ba34e4173a69b315e5514d5935125bcd6e398d2fcf4d1830d22784'
with tarfile.open(archive) as bundle:
    for member in bundle.getmembers():
        path = pathlib.PurePosixPath(member.name)
        assert not path.is_absolute() and '..' not in path.parts
        assert member.isfile() or member.isdir()
    bundle.extractall(root, filter='data')
PY
python docs/evidence/gh354-query-path/probe.py \
  --output /tmp/am-gh354-query-probe \
  --binary /tmp/am-gh354-query-probe/fsqlite
```

The default runs the reservation pair, message pair, and full-query plan capture
as separate invocations, matching the successful recorded protocol. Each child
has a 1 GiB address-space cap, 25-second CPU limit, and 30-second wall timeout.
Each source statement has two diagnostic marker SELECTs; a timing invocation
therefore contains 14 source queries and 28 marker queries. The markers identify
outputs and timers; their costs are excluded from source samples.

To construct and verify the fixture and SQL hashes without native execution:

```bash
python docs/evidence/gh354-query-path/probe.py \
  --output /tmp/am-gh354-query-probe --prepare-only
```

Use `--mode timing-reservation`, `--mode timing-message`, or `--mode plans` to
repeat one successful lane. New `.sql`, `.stdout`, `.stderr`, and `.json` files
are retained under the supplied output directory. Full raw CSV remains in those
files; future results must be assessed on their own, rather than copied into
this historical record. The harness refuses changed SQL or binary hashes.

The isolated opcode capture can be reproduced against the generated fixture:

```bash
/tmp/am-gh354-query-probe/fsqlite --batch \
  /tmp/am-gh354-query-probe/mailbox-scale.sqlite3 \
  'EXPLAIN SELECT id, project_id, agent_id, path_pattern, "exclusive", reason, created_ts, expires_ts, released_ts FROM file_reservations WHERE id = 4741;' \
  'EXPLAIN SELECT reservation_id, released_ts FROM file_reservation_releases WHERE reservation_id = 4741;'
```

## Isolated negative controls

Five additional candidate-only checks each ran one source statement in a
separate native CLI process against the same full fixture. Every process exited
zero, emitted no stderr, completed within its 10-second wall timeout, and
matched the complete canonical SQLite projection. Each child also had a
25-second CPU limit and 1 GiB address-space cap. There were no warm-ups or
diagnostic SQL statements, for exactly five native source statements in total.

| Source | Literal parameters | Complete output | Process wall | Statement wall |
|---|---|---|---:|---:|
| Reservation, missing ID | `[9483, 32768]` | Empty | 0.285425 s | 0.084 s |
| Reservation, negative ID | `[-1, 32768]` | Empty | 3.650891 s | 0.180 s |
| Reservation, zero byte cap | `[4741, 0]` | Empty | 3.948284 s | 0.803 s |
| Message, missing ID | `[8242, 4194304, 1024]` | Empty | 0.042450 s | 0.026 s |
| Message, zero payload cap | `[4121, 0, 1024]` | Three delivery witnesses, no message header | 0.037581 s | 0.008 s |

Message queries retain `LIMIT 1026`. The zero-payload result preserves the three
`row_kind = 1` witnesses for Agent026/to, Agent031/cc, and Agent036/bcc, each with
`read_ts = 7` and `ack_ts = 11`; there is no `row_kind = 0` message header. All
24 cells of each witness matched the canonical reference, with the same CSV
type/NULL limitations as the original measurements. `results.json` retains the
complete projections, exact stdout, exit/error status, and both process and
statement CPU accounting for all five controls.

These are correctness checks. Their timing is retained as diagnostic context
for bounded completion and is not added to the benchmark samples or ratios.
The successful controls do not establish the cause of either earlier timeout.
No legacy native controls were needed because every candidate result matched
its canonical reference.

The following reproduces these five statements separately after preparing the
fixture and verified binary above. It uses the same extracted SQL and literal
parameters, and stops if a process fails or times out:

```bash
python - <<'PY'
import pathlib, re, resource, runpy, subprocess, time
root = pathlib.Path('/tmp/am-gh354-query-probe')
probe = runpy.run_path('docs/evidence/gh354-query-path/probe.py')
queries = probe['extract_queries']()
cases = [
    ('reservation_candidate', [9483, 32768]),
    ('reservation_candidate', [-1, 32768]),
    ('reservation_candidate', [4741, 0]),
    ('message_candidate', [8242, 4194304, 1024]),
    ('message_candidate', [4121, 0, 1024]),
]
def limits():
    resource.setrlimit(resource.RLIMIT_AS, (1024**3, 1024**3))
    resource.setrlimit(resource.RLIMIT_CPU, (25, 25))
for name, values in cases:
    sql = re.sub(r'\?([123])\b', lambda match: str(values[int(match[1]) - 1]), queries[name])
    script = '.mode csv\n.headers on\n.timer on\n' + sql + ';\n'
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    result = subprocess.run(
        [str(root / 'fsqlite'), '--batch', str(root / 'mailbox-scale.sqlite3')],
        input=script, text=True, capture_output=True, check=True,
        timeout=10, preexec_fn=limits,
    )
    elapsed = time.monotonic() - started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    print(name, values, 'wall/user/sys:', elapsed,
          after.ru_utime - before.ru_utime, after.ru_stime - before.ru_stime)
    print(result.stdout, end='')
    assert not result.stderr, result.stderr
PY
```

## Unsuccessful attempts and outstanding qualification

A larger combined probe (86 requested source/plan statements) timed out at
60 seconds. Its uncaught Python timeout prevented retention of partial stdout.
A separate 18-query reservation ID/byte-refusal matrix timed out at 30 seconds
and captured no complete output. Neither attempt receives passing credit;
zero captured output does not establish zero executed queries. Their causes
were not established. The bounded matrix can be retried with
`--mode matrix-reservation`; a message matrix is also available separately.

These timeouts do not invalidate the separately completed, exit-zero timing
runs, but the successful timing samples cover only their named source IDs.
This evidence cannot close server-idle CPU, repair convergence, publication
contention, retention behavior, or concurrent-write acceptance. Record native
Rust and integration qualification separately when it is actually executed.
