# Dependency Upgrade Log

## September 17, 2026 — FrankenSQLite 0.4.4 migration (in progress)

Tracked by `br-5lgwn`. This section supersedes historical current-status
statements below; v0.3.36 is published and this work targets unreleased main.

September 19 WinSafe update: the Windows-only
kernel dependency moves from exact 0.0.28 to exact 0.0.29, published from
`71ed88c2a0d18b03ee452f6d4261f4c22f443483`. The consumed `MoveFileEx`,
`ReplaceFile`, flags, errors and UTF-16 conversion are unchanged. The new
optional multimedia feature is not enabled; the lockfile changes only
WinSafe's version and checksum. Audit reports zero vulnerabilities and the
existing unmaintained/unsound warnings. Workspace/all-target check, strict
Clippy, formatting and the remote Windows cross-build passed. Native
SURFACEBOOKJE execution passed 1,873 core tests with one existing ignored
test in 22 seconds, including move collisions, NUL rejection, leaf/symlink
replacement races and partial-move rollback. The executed SHA-256 is
`4f21dac647f6c391e1351450e4481ffd447daec05b4b7de71d7fb10600e970de`;
`044-winsafe-029-native-corrected.json` and its complete artifacts retain
the receipt. The initial launcher path-quoting failure ran no tests and is
preserved separately. This qualifies the dependency, not Windows publication
crash durability or the full release.

September 19 `dirs` update: the workspace now uses 7.0.0, and the CLI and
core crates inherit that pin instead of independently requiring version 6.
Upstream's sole source behavior change moves Windows `preference_dir()`
from local to roaming application data; this project does not call that API.
Resolution redirects five application edges to the already-present version 7;
transitive consumers retain version 6. The parked, non-workspace local
agent-detection crate remains outside this upgrade. All 1,960 core tests passed
(two existing skips) through strict RCH at 21:19 UTC, and formatting passed.
`044-dirs-700-audit.json` reports zero vulnerabilities and the existing
unmaintained/unsound warning categories. Workspace/all-target check and strict
Clippy passed on the warm remote worker at 21:29 and 21:33 UTC
(`044-dirs-700-{check,clippy}-rehomed.log`). The original queue-timeout refusal
and the canceled queued replacement remain recorded; neither executed locally.
These results qualify this dependency change, not the complete release.

September 19 FTUI update: all seven direct family
dependencies move from 0.5 to 0.7.0 together, resolving fifteen FTUI packages
from upstream revision `798efa0bb746601cea78b75ad8bc859f738a6456`.
The console now reads `TerminalCapabilities::color_depth`; the existing
capability regression also rejects RGB support for Mono, ANSI16 and ANSI256.
The facade's new default enables both backends, so it instead explicitly
enables runtime/extras and retains the application's existing platform backend
selection. The optional Asupersync executor remains disabled. The lockfile
changes no packages outside FTUI; two FTUI edges now use the already-resolved
base64 0.23.1. Audit reports zero vulnerabilities with the previous warning
categories. Workspace/all-target compilation, strict Clippy and formatting
passed remotely (`044-ftui-070-check-final.log`, `044-ftui-070-clippy.log`,
`044-ftui-070-fmt-final.log`). All 3,392 selected console, TUI and golden
tests passed in 49.985 seconds at 22:22 UTC, with no snapshot updates
(`044-ftui-070-runtime-resumed.log`; 1,578 tests outside the selection).
The initial run reached the SSH test timeout during compilation and supplied
no runtime verdict; its log is retained. The resumed run used the correct
`RCH_TEST_TIMEOUT_SEC` override and the same worker cache. These results
qualify the FTUI upgrade, not the full release.

September 19 Beads prerequisite review: published `beads_rust` 0.6.0 keeps
the consumed discovery/routing APIs, but its `BEADS_DB` environment override
takes precedence over our validated `BEADS_DIR`. A real two-workspace control
with the official 0.6.0 binary returned the foreign workspace's issue. The
application now clears that override along with `BD_DB` and `BD_DATABASE`.
The fresh-child regression then exposed a second integration defect: current
`list` output wraps rows in `issues`, but the application silently treated
that object as an empty list. The shared decoder now handles the list envelope
and the ready command's array (`br-8nski`, `br-q0b1m`). All eight selected
tests passed against the real 0.6.0 binary, including independent workspaces
in a fresh child and existing ready/status handlers
(`044-beads-authority-envelope-runtime.log`). Final workspace/all-target
check, strict Clippy and formatting passed. Earlier test-shape and
production-decoder failures remain in the evidence logs. The embedded
dependency now uses qualified 0.6.0. All 24 selected discovery, wrapper,
real-database handler and doctor-probe tests passed at 20:55 UTC
(`044-beads-060-runtime.log`, strict RCH exit 0). Workspace/all-target
check, strict Clippy and formatting passed in `044-beads-060-{check,clippy,fmt}.log`.
Targeted
resolution changes Beads, its exact Asupersync 0.4.9 dependency to 0.4.10,
and adds `tar` 0.4.46; the mailbox retains Asupersync 0.5.0. Version 0.6.0
still requires the separate patched 0.3.18 engine. Default features remain
disabled to avoid its optional self-update HTTP runtime. The new graph's
audit reports zero vulnerabilities, with existing warning classes retained
in `044-beads-060-audit.json`.

September 19 Blake2 update: the direct release-verifier dependency
advances from 0.10.6 to 0.11.0. Published upstream changes replace aliases with
newtypes and move to digest 0.11; both application consumers use the unchanged
`Blake2b512::digest` interface and no removed feature or variable-output type.
Version 0.11.0 already existed transitively, so resolution changes only the CLI
dependency edge. Argon2 retains its required 0.10.6 dependency; an initial
attempt to replace that shared version was rejected before any lockfile change.
All 14 real signed-release fixture and tamper/key-rotation tests passed in
`044-blake2-011-runtime.log` (strict RCH exit 0, September 19 19:44 UTC).
Workspace/all-target check and strict Clippy passed on this dependency graph;
later setup changes still require their own final qualification. No package
versions were added to the audited graph by this direct-edge update.

September 19 TOON update: `tru` 0.2.4 replaces 0.2.3 after review of the
published package and upstream maintenance notes. Default features remain
empty; no optional asynchronous runtime is enabled. The targeted resolution
adds only `tru` 0.2.4 and removes its predecessor plus 58 obsolete/duplicate
packages, reducing the graph from 880 to 822 packages. Its build metadata now
shares the existing `vergen-gix` 10 stack. The remaining semantic edge changes
select already-present Windows bindings and `gix-imara-diff`'s hashbrown;
native Windows qualification remains required. The corrected old-version
baseline passed all 95 selected tests at 18:20 UTC; the new version passed
all 95 selected tests at 19:02 UTC (strict RCH exit 0,
`044-tru-024-runtime.log`), including the strengthened round-trip oracle.
Workspace/all-target compilation and strict Clippy
passed on the new graph (`044-tru-024-{check,clippy}.log`); the isolated
format check also passed after correcting the test assertion's wrapping
(`044-tru-024-fmt-corrected.log`). Details: `044-tru-024-{metadata,resolution}.log` and
`044-tru-024-semantic-edge-review.json`. The refreshed audit reports zero
vulnerabilities; existing `paste` unmaintained and `lru` unsound advisories
remain visible in `044-tru-024-audit.json`.

Parallel reliability work (`br-8r6dl`, September 19): a real 0.4.4 VFS
regression reproduced descriptor growth from 1 to 65 after 64 reopen/close
cycles while preserving foreign-process lock exclusion. An isolated Linux
reuse candidate now passes all 433 VFS library tests, including that bound,
foreign lock exclusion/final unlock, path replacement and exclusive-create
refusal (strict RCH exit 0 at 07:05:28 UTC; artifact
`044-descriptor-reuse-vfs-corrected.log`). It uses a Linux O_PATH inode
witness and effective-access/canonical-mode checks for an existing retained
lock domain. Other platforms retain the existing implementation. Engine
workspace/all-target check and strict Clippy both passed in
`044-descriptor-reuse-*.log`;
the final candidate additionally passes permission-revocation coverage on
the non-root Linux worker. It is now published at `db458bfba780e79d099d9f8986da5a1f7b360901`
on the isolated `am-runtime-044-20260919` branch and pinned by all twenty
application 0.4.4 aliases. Neither a deployed fix nor cross-platform closure
is claimed.

September 19 application follow-through: all four real NOCASE backup/export
regressions passed on `dbcc7adb` (strict RCH exit 0 at 07:21:34 UTC,
`044-gh326-nocase-fixed-runtime.log`). The subsequent broad DB gate passed
3,076 tests, failed one sibling-discovery rollback test, skipped seven and
left 258 unrun. The valid 13-parameter query exposed an engine replay bug:
`INSERT ... SELECT ... ON CONFLICT DO UPDATE` retained original UPSERT
parameter indices while its per-row replay supplied only inserted values.
Engine commit `24ae22d` repairs global UPSERT/RETURNING binding before row
replay, including attached targets. Its real SQLite oracle first reproduced
the failure, then all six target tests passed at 15:58 UTC. All 433 VFS tests
passed at 15:59 UTC; engine workspace/all-target check passed at 16:07 UTC
and strict Clippy at 16:24 UTC after correcting a test's permission literal
from `0` to `0o0`. Final source hashes were verified before publication.
The application query's identity, TTL and review-race checks are unchanged.
All twelve application sibling-discovery tests now pass on the new pin,
including the original binding failure and real reopen/persistence case
(strict RCH exit 0 at 17:04 UTC, `044-upsert-app-sibling-runtime-j4.log`).
The broader DB/schema/migration/search suite passed all 3,335 tests with
seven existing skips at 17:27 UTC (strict RCH exit 0,
`044-runtime-pin-db-broad-quiet-extract.log`). This clears the application
UPSERT blocker; whole-release runtime and platform validation remain open.
The first broad run was interrupted after filesystem journal waits exceeded
three minutes. Its compiled four-binary nextest archive was transferred
byte-identically to a quiet worker and executed with the original assertions
and watchdogs, completing in 161.843 seconds. The archive SHA-256 is
`7002b7d2556cec1c1aa3496a845dc8a42ecdca4b958cae452f16d5cf05b803be`.

The targeted `db458bf` resolution preserves the complete package/version
inventory and changes exactly twenty engine sources. Cargo additionally
reselected sixteen dependency references in fifteen existing packages:
Windows bindings, tempfile's getrandom and prost-derive's itertools.
Published local manifests allow these selections; native Windows validation
remains pending. No forbidden runtime package was introduced. The new-pin
workspace/all-target check passed on ovh-a at 16:51 UTC (strict RCH exit 0,
`044-runtime-pin-app-check.log`). Strict Clippy then found one overlong ATC
population integration test. Its bounded-hydration loop was extracted into
a helper without changing any assertion, and the full workspace/all-target
Clippy rerun passed at 16:55 UTC (`044-runtime-pin-app-clippy-helper.log`).
Formatting and the final post-extraction workspace check pass. All six real
ATC integration tests passed at 17:55 UTC on the quiet worker, including
940-agent bounded hydration and liveness behavior (strict RCH exit 0,
`044-atc-helper-runtime-quiet.log`). The archive was built on hz3 and verified
byte-identical before execution; SHA-256
`7fa44c7f37141ca0fc2d11a1c3d3b367c651a42fb4ae304f069d2bc3333ee1d4`.
The older-pin TOON baseline
completed with twelve passes, one failure and 82 unrun tests. Its new
round-trip assertion incorrectly distinguished JSON integers from the
decoder's documented `f64` number representation. The corrected test
requires the exact integer array on the wire and compares decoded values
with explicit floating-point expectations for those three numbers, retaining
all other field and no-JSON-fallback assertions. A fresh 95-test baseline
on the current engine pin and unchanged `tru` 0.2.3 passed all 95 tests; the original
failure is retained in `044-toon-baseline-runtime.log`. The
focused sibling-discovery gate passed on hz3 with four normally admitted
compiler slots, reusing the interrupted one-slot build's cache. The initial
attempt exited 143 without a test verdict; its log is retained.

The earlier application workspace/all-target check passed at 07:59 UTC. Strict
Clippy then found a missing semicolon in ATC hydration; the manual fix is
committed as `fd7a8f5b`. Fresh Clippy admission on hz3 was initially refused
because its Rust inventory probe cannot acquire its cache lock. These are
pre-compilation refusals, not compiler failures or passes; the later
successful ovh-a compiler gates above supersede these attempts.

- Verified the published `fsqlite` 0.4.4 dependency metadata and tag commit
  `9d3d98778a372aba95d76d05c5c974ac0238c96a`.
- Inventoried 95 direct registry dependencies; receipt:
  `/data/projects/am-release-20260912/library-inventory-20260917.json`.
- Migration requires SQLModel 0.5.0 and Asupersync 0.5.0. FastMCP 0.10.0
  provides the matching runtime; its breaking HTTP/context changes require
  real transport regression coverage. FrankenSearch now follows the 0.5
  runtime; embedded Beads retains its separate 0.4 runtime graph.
- Validation is pending. No compatibility or release claim is made yet.
- The mailbox dependency edges resolve patched `fsqlite`/`fsqlite-types`
  0.4.4, SQLModel 0.5.0 and Asupersync 0.5.0. Embedded Beads 0.5.4 still
  requires its separate 0.3.18 engine; its qualified git patches remain.
- FastMCP registry 0.10.0 predates GH321 proposal negotiation. Preserve that
  behavior using the matching 0.10.0 family at immutable upstream revision
  `1c2e5e4b61a839ebcc430e7bacbc94d4d3205102`, which includes the repair.
- Independent source review confirms the Windows namespace identity repair
  remains in 0.4.4. The new `.fsqlite-shm` mapping is not called by ordinary
  `Connection::open`/SQLModel workers; recovery inventory must be reviewed
  again when upstream connects that mapping to the public connection path.
- Initial audit finds pre-existing `RUSTSEC-2026-0285` in rustls 0.23.43;
  upstream 0.23.45 fixes it. This update is queued after the runtime gate.
- First RCH check failed before compilation with SSH exit 255. A second
  worker is compiling; both logs are retained under the artifact directory.
- The initial remote compiler result exposed two migration errors: removed
  SQLModel `test_on_return` builder and a shared reranker `Cx` type crossing
  Asupersync 0.4/0.5. Removed the obsolete pool setting; moved FrankenSearch
  to immutable `dd093fb230404ab08be2ed6f27776ed6c4796485` (facade 0.6.0,
  component crates 0.3.0, rerank 0.4.0). Required dependency floors also move
  tokenizers to 0.23.2, wide to 1.7.1, and registry Tantivy to 0.26.2.
  Container and source-installer provenance pins follow the same revision.
  The pinned direct Tantivy 0.27 dependency is preserved.
- The second admitted workspace/all-target check compiled the production
  workspace, then failed on a pre-existing storage test closure taking zero
  arguments instead of the helper's path argument. Corrected the closure to
  `|_|`; a fresh all-target check is still required. Subsequent database
  runtime results and their repairs are recorded below.
- Manually corrected existing formatting drift in ten source files without
  changing behavior. `cargo fmt --check` and `git diff --check` now pass.
- Static UBS comparison of the pool migration found no new diagnostic messages;
  the existing baseline is not clean (52 critical and 4255 warning findings).
- Further research: dirs 7 changes Windows `preference_dir` to roaming data;
  this project does not call that function. The update remains queued for its
  own test gate, alongside the remaining inventoried libraries.
- Compatibility risk found before qualification: exact 0.4.4 omits the former
  `c76b22ec557344bf173f61e8f63580670d99ee62` CREATE TABLE schema-normalization
  and prepared UPDATE/DELETE parameter-numbering fixes. The older WAL repair
  `dedf3e1be376b9b8bd90e458912aa9b226b86bf9` is retained. These are source
  findings, not executed failures; the existing recipient timestamp invariant
  regression and canonical ALTER TABLE test must determine whether the new
  engine needs the narrow SQL compatibility patch carried forward.
- First real database run completed compilation on `vmi1152480` at 16:16 UTC:
  28 passed, one failed, six skipped; fail-fast left 3234 unrun. SQLModel 0.5
  reads a `checksum` column missing from our migration ledger. Added an
  idempotent ledger-column upgrade, preserved historical empty checksums,
  recorded checksums for new migrations, and rejected drift before pending
  SQL or the completed-ledger fast path. Added real canonical SQLite tests;
  focused remote validation is running in `044-schema-and-bind-runtime.log`.
- Focused rerun at 16:26 UTC passed the original checksum failure and both new
  checksum tests. The recipient invariant reproduced the missing engine fix:
  UPDATE returned zero affected rows instead of one. Carried the original
  `c76b22` patch unchanged onto exact 0.4.4 as
  `4b1ffc77cc5c2ed6745cc37955c3a7789e178a47`, published only on the candidate
  branch `am-sql-044-candidate-20260917` for reproducible remote testing.
  The 20 mailbox engine packages now use that immutable 0.4.4 revision; Beads'
  older graph remains separate. No crate release or main-branch push occurred.
  Added an application-level FrankenSQLite/canonical ALTER/reopen regression.
  Patched validation completed at 17:13 UTC in `044-patched-sql-runtime.log`:
  all five selected tests passed remotely (3251 skipped), remote exit 0.
  This includes checksum preservation/drift, the original ATC failure,
  prepared binding, and runtime CREATE/canonical ALTER/runtime reopen.
  Broader database coverage is running in `044-patched-db-broad-corrected.log`.
  The first broader invocation named a nonexistent search integration target;
  it ran no tests. Corrected it to `search_v3_conformance` and retained the log.
- The corrected broad gate completed at 17:28 UTC: **3271 passed, 7 skipped**,
  strict RCH remote exit 0. Database library, schema migrations, stateful
  invariants and Search V3 conformance all passed on the patched 0.4.4 graph.
- Advanced rustls 0.23.43 to 0.23.45, the upstream fix for
  `RUSTSEC-2026-0285`; this lock update changed only rustls. Server HTTP/JWT/
  transport tests are running remotely in `044-rustls-transport.log`;
  refreshed audit output is `044-audit-rustls.json`.
- The refreshed audit exits 0 with zero vulnerabilities; existing `paste`
  unmaintained and `lru` unsound warnings remain, without suppression.
- Concurrent commits `944ffa74`, `bc7b0103` and `4694074d` captured the
  candidate source and dependency edits while validation continued. They
  do not imply completed transport or full-workspace qualification.
- Beads 0.6.0 research confirms its Asupersync 0.4.10 pin and separate
  FrankenSQLite 0.3.18 graph. This application's five call sites use only
  configuration directory discovery/redirect APIs. The optional FastMCP
  dependency and dev-only TOML pin are not enabled by this consumer;
  resolution and focused tests remain pending.
- The older all-target workspace check passed remotely on hz3 at 17:36 UTC,
  exit 0 (`044-check-hz3-refreshed.log`). It predates the checksum/engine
  repairs and rustls update, so a current-source check is running separately
  in `044-check-patched-current.log`.
- That current-source attempt stalled in source transfer before Cargo started.
  Retry is `044-check-patched-retry.log`; no local fallback was used.
- The retry passed at 18:29 UTC: current-source workspace/all-target check,
  strict RCH remote exit 0. Strict workspace/all-target Clippy is running
  separately in `044-clippy-patched.log`.
- Initial Clippy admission reported a missing runtime without executing.
  Native rustup confirmed the installed Clippy component; refreshed RCH's
  live capability cache and retried as `044-clippy-refreshed.log`.
- The refreshed and explicitly pinned attempts still refused. Diagnostic
  `044-clippy-diagnose.json` reports a missing component despite the live
  inventory containing it. The same diagnostic admits ovh-a, so Clippy is
  now submitted there as `044-clippy-ovh.log`; no admission override used.
- Rustls transport gate completed 18:51 UTC: **149 passed**, 4572 skipped,
  strict RCH remote exit 0. Its snapshot predates concurrent merge
  `29d6edc5` (17:32 UTC); the successful 18:29 compiler check and active
  Clippy include that merge. The next server gate repeats transport coverage
  on merged source alongside the sanitizer update.
- Ammonia 4.1.4 → 4.2.0: reviewed upstream release changes (HTML5ever 0.40,
  CSS parser 0.38, MSRV 1.85, additive introspection APIs). Updated the
  manifest floor and resolved lockfile; Markdown/XSS and transport tests
  will qualify this dependency before the next update.
- Ammonia's warmed worker refused an unrelated skillranker scratchpad alias
  during hard preflight; a live probe confirmed the issue. Left foreign
  state untouched and moved to hz3. Cancelled the first hz3 job during
  source sync after noticing its missing isolation runner; created a new
  private runner and resubmitted as `044-ammonia-isolated.log`.
- hz3 then reported missing cargo-nextest before any tests. Provisioned the
  same nextest 0.9.143 executable used by the passing remote gate into the
  private tool directory; source/copy SHA-256 both
  `d97959a56feb7a1297576c1b47d9aaf7b5badd6be596e695ed203fa4490884ea`.
  Its version command succeeds. Retried as `044-ammonia-provisioned.log`.
- Strict workspace/all-target Clippy completed on ovh-a at 18:57 UTC,
  remote exit 0 with `-D warnings` (`044-clippy-ovh.log`). This includes
  merged source `29d6edc5` and rustls 0.23.45, but predates Ammonia 4.2.0.
- Rustix remains 1.1.4: the preserved FastMCP revision's workspace manifest
  requires `=1.1.4`, preventing selection of the semver-compatible 1.1.5.
  Do not alter the qualified upstream git revision merely to loosen its pin.
- A fresh formatting check found layout drift in six files introduced by
  merge `29d6edc5`. Corrected the layout manually, preserving behavior;
  workspace `cargo fmt --check` and `git diff --check` pass at 19:07 UTC.
  These formatting edits postdate the active Ammonia snapshot and will be
  included in subsequent final checks.
- Audit of the Ammonia candidate lockfile exits 0 with zero vulnerabilities
  (`044-audit-ammonia.json`); the same existing `paste` unmaintained and
  `lru` unsound warnings remain. This does not substitute for runtime tests.
- Prepared an isolated doctor/WAL runner on ovh-a, but RCH refused the
  separate CLI gate before execution because that worker now has critical
  memory pressure (`044-doctor-merged.log`, RCH-I002). No local fallback
  or pressure override. The already-admitted Ammonia gate continues on hz3.
- Concurrent commit `f0bc676c` captured the Ammonia candidate and manual
  formatting corrections while its tests were still compiling. The commit
  is not a completed qualification result; the runtime gate remains open.
- Ammonia qualification completed at 21:08 UTC: **297 passed, 4430 skipped**,
  strict RCH remote exit 0 (`044-ammonia-provisioned.log`). Includes Markdown
  sanitization/XSS, HTTP/JWT/transport, and merged ACK-TTL regressions. The
  cold build took 127 minutes; tests took 122 seconds. Ammonia 4.2.0 retained.
- Next: jsonwebtoken 11.0.0 → 11.1.0. Upstream adds an opt-in insecure
  claims decoder; this project continues using verified decoding. Run the
  existing authentication acceptance/rejection regressions after resolution.
- JWT resolution changes only jsonwebtoken. First runtime submission refused
  because hz3 now has zero slots (`044-jwt-runtime.log`, RCH-I003). A fresh
  vmi1152480 probe reports its former foreign-alias blocker repaired and
  `projects_root_ok: true`; submitted the warm-cache test there instead
  (`044-jwt-warm-worker.log`). No fleet configuration changes by this task.
- The recovered worker is rebuilding broadly in its current Cargo artifact
  layout despite the earlier target directory. JWT tests have not executed
  yet; session 60291 is active. Do not interpret the resolved lockfile as
  a passing JWT update or a completed general library refresh.
- Resumed the pending doctor/WAL library regressions on hz3 after it regained
  one admissible slot at 22:27 UTC (`044-doctor-hz3.log`, session 71926).
  This runs independently of the JWT gate on vmi1152480; no shared-worker
  build contention or additional dependency update was introduced.
- JWT 11.1.0 passed **64 authentication tests**, 4663 skipped, strict RCH
  exit 0 at 22:39 UTC September 17 (`044-jwt-warm-worker.log`).
- Doctor/WAL gate passed **1225 tests**, 1616 skipped, strict RCH exit 0
  at 01:37 UTC September 18 (`044-doctor-hz3.log`). These source snapshots
  predate subsequent integrity/cache commits merged in `1e3dd21d`; final
  current-source verification remains required.
- Coordination recovery at 06:15 UTC September 18: the live Agent Mail
  process had 2047 descriptors against soft limit 2048 (hard 1048576),
  causing sidecar-open errors. Raised only PID 1685075's soft limit to 8192,
  retaining its hard limit; inbox/reservations succeed again. No restart,
  database repair or file mutation. This mitigates exhaustion, not its cause.
- Next: Clap 4.6.6 → 4.6.7. Upstream adds an opt-in lazy-subcommand derive
  attribute; no default behavior change requested. Qualify parser/help tests.
- Clap and its builder/derive crates now resolve to 4.6.7. The strict remote
  parser/help gate is running on hz3 (`044-clap-runtime.log`). Cargo also
  reselected existing Windows dependency edges within their published ranges
  (for example errno/tempfile allow `>=0.52, <0.62`, dirs-sys allows `>=0.59`).
  These are resolver changes, not evidence that the previous edges were invalid;
  Windows qualification remains required.
- Descriptor accumulation is tracked separately by `br-8r6dl`: a subsequent
  live-process sample found 2080 descriptors for the main database. Candidate
  0.4.4 retains redundant opens while inode lock claims exist, a possible
  explanation requiring reproduction. Never close these handles by force:
  classic POSIX locks can be lost by closing another descriptor for that inode.
- Current Clap-candidate audit returned exit 0 with zero vulnerabilities
  (`044-audit-clap.json`); existing unmaintained/unsound warning categories
  remain. New shared commits through `94edf0c3` introduced formatting drift
  in twelve files. Manual formatting-only corrections restore a passing
  workspace `cargo fmt --check` (`044-format-corrected.log`). No behavioral
  qualification of those new recovery/ATC changes is implied.
- Formatting-only commit `a09ff284` contains those twelve files. Compared
  each corrected file byte-for-byte with rustfmt's stdout for its HEAD
  baseline; all match. No formatter wrote source files. Released the exact
  formatting leases; pending dependency/tracker/log changes stay separate.
- Clap 4.6.7 passed **192 parser/help tests**, 2650 skipped, strict RCH
  exit 0 at 09:28 UTC September 18 (`044-clap-runtime.log`). This qualifies
  the dependency on its captured source; subsequent recovery/ATC merges
  still require final current-source gates.
- IndexMap 2.14.0 → 2.14.2 resolves without other package-version changes.
  Reviewed upstream macro-hygiene/const-initialization changes and the direct
  conformance fixture maps plus DB serialization consumers. DB tracking,
  cache, ordering, serialization and search-conformance tests are running
  strictly remotely (`044-indexmap-runtime.log`); no result yet.
  Cargo also reselected gix-imara-diff's hashbrown edge from existing 0.17.1
  to existing 0.15.5 within its `>=0.15, <=0.17` requirement; the final
  storage tests must cover this resolver change. Locked metadata contains
  880 packages and none of the forbidden async runtimes/HTTP stacks.
- Reviewed remaining `RUSTSEC-2026-0253` exposure in the resolved graph:
  the sole consumer of lru 0.16.4 is registry Tantivy 0.26.2 through
  FrankenSearch. Its only `LruCache` is `LruCache<usize, Block>` in
  `src/store/reader.rs`; the advisory's panicking-key-destructor trigger
  does not match these integer keys. This is a bounded source assessment,
  not an advisory suppression or a claim that the dependency is patched.
  The direct pinned Tantivy and engine crates use fixed lru 0.18.2.
- Queued a conditional IndexMap follow-up for the actual cache golden,
  conformance fixture loader and storage coalescer tests. It waits for the
  primary gate's terminal RCH success, stops on failure/refusal/deadline,
  and keeps remote-only execution (`044-indexmap-consumers.log`). This
  supplements indirect serialization coverage; neither run has passed yet.
- IndexMap qualification completed: **275 tests passed** (3019 skipped)
  at 15:15 UTC, followed by **54 direct-consumer/storage tests passed**
  (3665 skipped) at 15:16 UTC September 18. Both strict RCH runs exited 0;
  receipts are `044-indexmap-runtime.log` and `044-indexmap-consumers.log`.
  These supersede the pending status above; final workspace/platform gates
  remain separate. The lockfile change was already captured by shared
  commit `4c56a97a`; no duplicate implementation commit is needed.
- SmallVec 1.15.2 → 1.16.1 resolved alone. Reviewed upstream 1.16 changes
  (push implementation, debugger metadata and warning fixes), and the real
  messaging/contact/reservation consumers. A bounded sequential campaign
  will test this candidate, then update/test toml_edit 0.25.15 and tru 0.2.4
  individually. Those patch releases were researched in advance; no API
  migration is required by their release notes. Each stage requires the
  preceding strict remote tests to pass and fresh exclusive file leases.
  Unexpected package changes, merge conflicts or any failed command stop
  the sequence for review. No source-code rewriting or publication occurs.
- The SmallVec gate stopped with strict RCH exit 100 at 18:46 UTC
  September 18: **163 passed, one timed out**, 153 selected tests unrun.
  `send_message_reply_is_bounded_and_db_durable_with_async_archive` hit
  the unchanged four-minute nextest limit. TOML-edit and TOON were not
  updated. Seven phase markers now identify setup/send/flush progress;
  a focused remote strace run is pending in
  `044-smallvec-ack-fast-diagnostic.log`. This is not yet attributed to
  SmallVec, and neither the dependency nor the test is qualified.
- A bounded background observer waits for the diagnostic's terminal RCH
  result and collects phase markers plus only futex/flock/fcntl trace tails
  into `044-ack-fast-observation.txt`. It does not collect read/write payloads,
  rerun tests, alter worker security settings, or repair the live mailbox.
- The focused diagnostic completed September 19 at 01:40 UTC with strict
  RCH exit 0: one passed in 3.815 seconds, 974 skipped. No hang reproduced
  and no production fix is claimed. The full 317-test consumer repeat uses
  the ordinary isolated runner (`044-smallvec-runtime-repeat.log`); the
  previously timed-out test passed there in 1.032 seconds. The original
  timeout remains retained evidence, pending the complete repeat result.
- During the repeat, the numeric root-thread test stalled for 79.063 seconds
  and then passed. At 01:53:54 UTC its FrankenSQLite worker was waiting in
  filesystem journal paths (`jbd2_log_wait_commit` / `wait_transaction_locked`),
  with host I/O full-pressure avg10 at 57.25%. Read-only evidence is retained
  in `044-smallvec-repeat-io-observation.txt`. This explains an observed
  storage wait in this run, not the earlier unobserved timeout.
- Full SmallVec repeat completed at 01:56 UTC September 19: **317 passed,
  658 skipped**, strict RCH exit 0 in `044-smallvec-runtime-repeat.log`.
  Both slow persistence tests recovered with unchanged timeout limits.
  This qualifies the selected consumers; the original failed run is retained.
- Updated only `toml_edit` 0.25.13 → 0.25.15 (spec-1.1.0). Upstream patch
  releases reduce parser/render allocations; no API migration is needed.
  Lock diff contains only its version/checksum. The strict remote CLI gate
  includes TOML/config tests and both complete startup-timeout/legacy-launcher
  fixer modules, covering comment preservation and malformed-input handling.
  Result pending in `044-toml-edit-runtime.log`. TOON remains unchanged.
- Current candidate audit reports zero vulnerabilities, retaining existing
  unmaintained/unsound warnings (`044-audit-toml-edit.json`). Latest workspace
  formatting check found drift in 19 files introduced by intervening merges.
  Manual corrections are complete: all 19 files byte-match read-only rustfmt
  output from their HEAD originals, and workspace `cargo fmt --check` passes
  (`044-format-smallvec-final.log`). No behavior changes were made.
  The README limitation now agrees with the actual immutable Git dependency
  graph instead of requiring obsolete sibling checkouts on current main.
- A fresh read-only `cargo update --dry-run --verbose` preview identifies 77
  compatible package selections, including transitive changes beyond the
  direct-library inventory (`044-remaining-update-preview.log`). It changed
  no lockfile entries. Those candidates still require research and consumer
  validation; they have not been applied as a batch.
- The first TOML-edit gate stopped before test execution at 03:35 UTC
  September 19 (strict RCH exit 101). Newly merged ATC code declared
  `should_defer_refresh` const although the pinned compiler cannot call
  `VecDeque::is_empty` in a const function. Removed the unnecessary `const`;
  all callers use this predicate at runtime. The original failure remains in
  `044-toml-edit-runtime.log`. The corrected gate also selects the ATC
  population regressions (`044-toml-edit-atc-fix-runtime.log`); result pending.
- Fresh GH326 review found existing real NOCASE/export/backup regressions that
  postdate the earlier database receipt. Their strict remote run is compiling
  on a separate worker (`044-gh326-nocase-runtime.log`). The candidate still
  has the uppercase-folding registry comparator; upstream `6ede4b51c` fixes
  it, but its own commit explicitly records no executed Rust tests. The VDBE
  byte comparator also continues past a shared NUL, unlike canonical SQLite.
  These are source/oracle findings pending application-level reproduction;
  neither an engine pin change nor issue closure has been made.
- Prepared a narrow candidate in the isolated `fsqlite-044-research` clone:
  registry lowercase/shared-NUL handling follows upstream `6ede4b51c`, and
  the VDBE comparator now also stops at a shared NUL before comparing full
  lengths. A real rusqlite differential regression checks both paths across
  441 input pairs (ASCII punctuation, case, Unicode and embedded NULs).
  Two-file formatting passes. Strict remote execution is compiling on ovh-a
  (`044-nocase-engine-candidate.log`); this is not yet a qualified fix.
  Application pins are unchanged, and actual export/backup validation is
  still required after engine qualification.
- The initial comparator regression passed remotely on ovh-a at 05:44 UTC
  September 19 (1 test, 441 input pairs, strict RCH exit 0). Follow-through
  review found DISTINCT and in-memory UNIQUE keys also included bytes after
  NUL. The candidate now normalizes those suffixes while preserving original
  lengths and key framing; the oracle test additionally checks DISTINCT and
  real MemTable uniqueness. The original queued checks refused changed source
  hashes (exit 65). Fresh expanded tests/check/Clippy are running under
  `044-nocase-final-*.log`; the initial pass does not certify these additions.
- Strengthened the existing CLI TOON output test before its dependency update:
  it now rejects silent JSON fallback and decodes captured TOON to verify all
  values survive, including multiline/quoted/Unicode text and empty values.
  Formatting passes; runtime validation remains pending. `tru` stays at 0.2.3.
- The final expanded NOCASE runtime gate passed all 18 tests (2 function,
  16 VDBE), including the 441-pair comparator/DISTINCT/UNIQUE oracle, at
  05:47:27 UTC September 19 on ovh-a (strict RCH exit 0;
  `044-nocase-final-runtime.log`). Source hashes are retained in the sequence
  log. The workspace compiler check is now running, followed by Clippy.
  Application export/backup tests have not yet qualified this candidate.
- Engine workspace/all-target check and strict Clippy completed successfully
  at 06:03 and 06:16 UTC September 19, respectively, with matching source
  hashes and strict RCH exit 0. Committed the repair as
  `dbcc7adb5d2491504af4c07a38a58378522f5a07`, published the isolated
  `am-nocase-044-20260919` branch, and pinned all 20 runtime engine crates
  to that revision. No upstream main branch, tag or crate release changed.
- The application baseline reproduced GH326: public proactive backup rejected
  its staged native export during strict canonical health checks (1 passed,
  1 failed, 2 unrun at 06:16 UTC). The fixed-pin rerun uses `--no-fail-fast`
  and is active in `044-gh326-nocase-fixed-runtime.log`.
- The merged TOML/ATC/backup-admission gate passed 244/244 at 06:27 UTC.
  Its subsequent compiler command was refused before compilation because RCH
  estimated 16 cores on a 10-slot worker. A fresh application sequence uses
  explicit `-j1`: workspace check, strict Clippy, then CLI TOON/output tests
  (`044-nocase-app-*.log`). No local fallback was used.
- Cargo's targeted engine resolution also changed 16 dependency references
  within existing package versions, including Windows bindings, tempfile's
  getrandom reference and prost-derive's itertools reference. The initial
  assertion expecting only engine source changes correctly failed; detailed
  review is retained in `044-nocase-resolution-review.txt`. These resolver
  choices remain subject to the application and native-platform gates.
- Concurrent commits captured the ATC fix and corrected ambiguous empty-vector
  assertions in cleanup tests (`a27c2bda` merge). The first retry predates that
  cleanup correction. A bounded shell follow-up waits for that owned RCH
  process to exit, then validates the merged source with the TOML/ATC tests
  plus persistent backup-admission tests, followed by workspace/all-target
  check and Clippy only if each preceding stage passes. It does not mutate
  dependencies or publish. Logs: `044-merged-validation-sequence.log` and
  `044-merged-{toml-atc-runtime,check,clippy}.log`. Results remain pending.

## September 16, 2026 — release qualification update

Version remains 0.3.35; the next application release is not published. This
section supersedes the current-status statements in the historical September 12
notes below.

- The original WAL/doctor failures were repaired. The final September 12
  workspace run at `0259279b` passed **17,510 tests, zero failures, 39 skipped**.
  That result includes Comrak 0.55.0 and ChaCha20 0.10.2, but does not qualify
  the subsequent September 15–16 changes.
- FastMCP remains 0.7.1 with all eight packages pinned to immutable revision
  `c14d26b49f132625f4141b72669b157467b9aac2`. This backports protocol negotiation
  for GH #321 while preserving the exact Asupersync 0.4.9 runtime family.
  The fresh consumer passed 18 stdio assertions and real HTTP initialization
  for four proposed protocol versions, including subsequent tool discovery.
- FrankenSQLite remains 0.3.18 with all 20 packages pinned to immutable revision
  `2633b38a26bde68db23172b12aa402ed698cc309`, published as
  `am-sql-compat-20260916`. It retains the SQL/WAL compatibility corrections
  and fixes a Windows namespace probe that compared a file identity with its
  mutex. Native Windows namespace tests passed 42/42; Linux generation tests
  passed 3/3 and strict VFS Clippy passed. Adoption changed only the 20 engine
  source revisions in Cargo.lock; unrelated package versions and edges were
  preserved.
- Windows consumer qualification subsequently exposed Unix-only imports in
  archive recovery test helpers (br-dlurv). The helpers are corrected, but the
  rebuilt consumer and native recovery tests remain pending. Engine tests alone
  do not qualify the application.
- The current Linux full run selects 17,540 tests across 149 binaries and is
  still running. Current-pin rollback tests, final compiler gates, macOS native
  qualification, six-platform release binaries, signatures, installation, and
  venue publication remain open. The retained logs are under
  `/data/projects/am-release-20260912/`.
- The unadopted dependency candidates below remain deferred while release bugs
  are fixed. Their September 12 version research is historical, not a claim
  that every listed package is still latest or has been individually validated.

## September 12, 2026 — next release (br-xql74)

Status: in progress; no new version, tag, or publication yet. Baseline source
`5d2f6af863de1d021fb6ee862fb5f928f6168d0e`, version 0.3.35.
The older June entries below are historical evidence, not current release gates.

Live registry research covered 89 direct packages: 54 already current and 35
newer packages grouped into 18 coherent upgrade units. Preserve Tantivy's git
revision and the gated FrankenSearch path dependencies. FrankenSQLite 0.3.18
is already latest stable.

- Updated: Comrak 0.54.0 → 0.55.0. Fixes two autolink denial-of-service
  issues ([GHSA-xg9p-p4jc-c46g](https://github.com/kivikakk/comrak/security/advisories/GHSA-xg9p-p4jc-c46g)).
  No use of its deprecated `tagfilter` option was found. Strict-RCH Markdown
  regression selection passed 152 tests (5008 unselected), remote exit 0 on
  `am-release-css` at 2026-09-12 01:15:04 UTC. Full workspace nextest then
  started at source `046976941fa4933b8209decc591975c454713518` and finished
  with 17,484 passed, 19 failed and 39 skipped. Remote nextest exited 100;
  the outer RCH exited 103 because its post-run source receipt detected local
  changes and generated artifacts. This baseline has no frozen-source verdict.
- Pending, individually tested: indexmap 2.14.2; plist 1.10.1; smallvec 1.16.1;
  toml_edit 0.25.15; tru 0.2.4; franken-agent-detection 0.2.4;
  tokenizers 0.23.2; wide 1.7.0; blake2 0.11.0; dirs 7.0.0;
  winsafe 0.0.29; beads_rust 0.5.12; SQLModel family 0.4.3; FTUI family 0.7.0.
- Updated, whole-workspace validation pending: ChaCha20 0.10.1 → 0.10.2, replacing
  the yanked version and fixing an SSE2 portability defect. Only that package's
  version/checksum changes in Cargo.lock; an unrelated resolver change to
  tempfile's getrandom edge was manually restored, and `cargo tree --locked`
  accepted the preserved graph. The dependency is used through Asupersync and
  FrankenSQLite, not the share module's external `age` executable.
  The queued DB/conformance gate refused after the baseline exceeded the
  ten-failure circuit breaker; that queue ran no tests. After the user's
  instruction to fix the bugs, focused DB/search/cursor/load validation passed
  135 tests with this lockfile. The complete fixture-router test subsequently
  passed in the 14-test portability selection.
- Constrained: latest FastMCP 0.9.0 requires asupersync exactly 0.4.10 and
  Rust 1.100, while even latest beads_rust requires asupersync exactly 0.4.9.
  The workspace's Rust minimum was corrected to 1.100 as described below.
  Latest asupersync 0.4.11 satisfies neither exact pin. Keep the coherent
  existing runtime family; do not substitute mutable sibling checkouts.
- Compatible FastMCP candidate: 0.8.1 keeps asupersync exactly 0.4.9 and
  isolates `block_on` runtimes per thread, directly relevant to AM dispatch.
  The installed nightly is Rust 1.100.0-nightly and satisfies its floor.
  Existing explicit-context runner calls appear compatible; compile and real
  transport tests are pending. Both current and candidate FastMCP clients pin
  TOML exactly 1.1.4, preventing a standalone upgrade to TOML 1.1.6.
- Release gates pending: complete workspace tests, check, strict Clippy,
  formatting, security audit, six-platform portable builds, signed artifacts,
  installation/update checks, and applicable distribution venue verification.
- Corrected the workspace Rust minimum from 1.95 to 1.100, matching the
  already-selected FastMCP 0.7.1 manifest. The pinned nightly is unchanged.
  `cargo metadata --locked --no-deps` confirms all 12 active members inherit
  1.100; this is a metadata check, not a compiler or runtime test result.
- Remaining release failures include WAL-only CHECK violation setup, WAL-only
  schema-version visibility, structural-corruption authority with a live owner,
  live physical-copy admission, canonical-reader close/checkpoint visibility,
  and reconstructed-mailbox search freshness (repair validation below). The physical-copy guard preserves
  process-wide file locks. Existing assertions and that guard remain unchanged.
- Guarded live-search refresh repair: native live sources use namespace-bound
  read-only opens for the scan, all reopen retries and the final publication
  seal. Private snapshots remain query authority. Canonical/archive fallback
  sources cannot publish live index state. Failed refreshes retain private
  results with an explicit warning and actual bootstrap-error health; cached
  failure requires revalidation before success can be recorded.
  The original recovery test and guarded source-preservation tests passed in
  an 87-test remote selection. The new real writer-lock regression then caught
  false fresh health (88/89 passed); recording the actual refresh error passed
  all 89 tests. A separate stale-binary rerun omitted the two new tests despite
  synchronized source hashes and is excluded as final-candidate evidence.
  Final retry/recovery additions and the broader search-service selection
  passed all 232 tests through strict RCH (6796 unselected, remote exit 0).
  Source hashes match the frozen candidate. Workspace/all-target check and
  strict Clippy passed remotely at 13:50:16 and 13:52:22 UTC; formatting and
  diff checks passed. UBS remains reviewed nonzero: 119 critical, 4597 warnings
  and 1324 informational findings, with no suppression. Five other runtime
  failures still block release. Logs and SHA-256 receipts are retained under
  `/data/projects/am-release-20260912/live-refresh-*`.
- Fixed source-change retry and relevance cursor behavior passed 135 focused
  tests through strict RCH, including the real mutation-during-scan and
  pagination-under-writes regressions (3808 tests unselected, remote exit 0).
  The first repair run retained one pagination failure out of 99 tests before
  the cursor correction. Logs are in `/data/projects/am-release-20260912/`.
  The conformance/server/share portability selection passed all 14 tests
  (5959 unselected, remote exit 0), including the eight previously failing
  environment-sensitive tests. Its first build failed because the new fixture
  helper assumed SHA-1's digest implemented LowerHex; explicit byte encoding
  corrected that compile error. The rerun used the normal remote checkout,
  not a relocated source-content checkout. Publication remains blocked.
- The repaired candidate passed strict-RCH workspace/all-target `cargo check`
  and Clippy with `-D warnings` (remote exits 0 at 03:39:27 and 03:41:32 UTC).
  `cargo fmt --check` and `git diff --check` also passed. UBS remains a reviewed
  nonzero scan under the previously approved exception: 131 critical findings,
  1789 warnings and 436 informational findings; no suppression or clean-scan
  claim. The final digest-encoding correction was manually reviewed.
- Initial post-Comrak `cargo audit --json` completed with zero vulnerabilities.
  Audit success is separate from the focused Markdown test result above and
  does not establish whole-workspace runtime compatibility.
  This is not a clean security audit: separate warnings include unsound
  `lru 0.16.4` (RUSTSEC-2026-0253) through the gated FrankenSearch lexical
  crate's registry Tantivy 0.26.1, unmaintained `paste` and `rustls-pemfile`,
  and yanked `chacha20 0.10.1`. Compatible remediation is under investigation.
  Follow-up: ChaCha20 0.10.2 is compatible and fixes an SSE2 portability bug;
  its isolated update/test is pending. LRU's patched range is >=0.18.2;
  even Tantivy 0.26.2 retains `lru ^0.16.3`, and the gated source pins
  Tantivy exactly 0.26.1. Its sole `LruCache<usize, Block>` does not supply
  the advisory's panicking-key destructor trigger, but it remains unpatched.
  Preserve that constraint and warning rather than suppressing it or dropping
  the production lexical backend. A fix needs a separately reviewed immutable
  sibling revision or backport.
- Venue preflight: GitHub Actions permissions report `enabled=false`.
  Homebrew is already at 0.3.35 with four platform hashes. GHCR `latest` has
  amd64/arm64 images labelled 0.3.34 at source `0125f0505fa0ba604dbbbb580fad56d283e46488`.
  The three probed Agent Mail package names return crates.io 404 and every
  active workspace member remains `publish=false`; gated path/git dependencies
  still prevent treating this as an existing crates.io publication stream.


**Date:** 2026-06-11 | **Project:** mcp_agent_mail_rust | **Language:** Rust

## Summary

- **Updated:** local `/dp` dependency alignment, direct manifest floors, and compatible lockfile refreshes.
- **Skipped:** incompatible major-version lockfile upgrades constrained by upstream dependency ranges; crates.io publishing is intentionally disabled for all workspace members.
- **Failed:** none in the completed compiler, lint, and workspace test gates.
- **Needs attention:** binary release packaging and external distribution verification remain separate release gates.

## Updates

### Local /dp dependency alignment

- `fastmcp*`: `0.3.0` -> `0.3.1` to match `/dp/fastmcp_rust`.
- `sqlmodel*`: `0.2.1` -> `0.2.2` to match `/dp/sqlmodel_rust`.
- `ftui*`: `0.3.1` -> `0.4.0` to match `/dp/frankentui`.
- `beads_rust`: `0.2.6` -> `0.2.7` to match `/dp/beads_rust`.
- Removed the unused `sqlmodel-frankensqlite` patch entry because the active workspace graph does not depend on that package; keeping it caused Cargo patch warnings.

### 2026-06-11 local /dp refresh

- `asupersync`: `0.3.3` -> `0.3.4` to match `/dp/asupersync`.
- `beads_rust`: manifest constraint `0.2.10` -> `0.2.15` to match `/dp/beads_rust`.
- `franken-agent-detection`: `0.1.7` -> `0.1.8` to match `/dp/franken_agent_detection`.
- `frankensearch`: `0.3.0` -> `0.3.2` to match `/dp/frankensearch/frankensearch`.
- `frankensearch-core`, `frankensearch-embed`, `frankensearch-index`, `frankensearch-fusion`: `0.2.0` -> `0.2.1` to match the current local `/dp/frankensearch` crate versions.

### Compatible registry updates

- `tru`/`toon`: `0.2.2` -> `0.2.3`.
- `zip`: manifest floor tightened to `8.6.0` after lockfile resolution selected it.

### Manifest-level latest-stable updates

- `comrak`: `0.50.0` -> `0.52.0`.
- `crossterm`: `0.28.1` -> `0.29.0`.
- `getrandom`: `0.2.17` -> `0.4.2`; call sites now use `getrandom::fill`.
- `insta`: manifest floor tightened from `1.38` to the resolved current `1.47.2`.
- `json5`: `0.4.1` -> `1.3.1`.
- `plist`: `1.8.0` -> `1.9.0`.
- `sha1`: `0.10.6` -> `0.11.0`.
- `sha2`: `0.10.9` -> `0.11.0`.
- `similar`: `2.7.0` -> `3.1.0`.
- `tantivy`: `0.25.0` -> `0.26.1`.
- `tokenizers`: `0.22.2` -> `0.23.1`.
- `unicode-width`: `0.1.14` -> `0.2.2`.

### 2026-06-11 latest-stable direct registry refresh

- `git2`: `0.20.4` -> `0.21.0`; current source uses stable repository/status/diff APIs.
- `toml_edit`: `0.23.10+spec-1.0.0` -> `0.25.12+spec-1.1.0`; current source uses `DocumentMut`, `Item`, `Array`, and `value` APIs.
- `safetensors`: `0.7.0` -> `0.8.0`; no direct source call sites, optional dependency only.
- `wide`: `1.4.0` -> `1.5.0`; no direct source call sites, dependency surface only.

### 2026-06-11 compatible transitive refresh

- Ran `cargo update`, resolving 54 package changes including `chrono`, `dashmap`, `minijinja`, `regex`, `uuid`, `wasm-bindgen`, `zerocopy`, and related transitive crates to their latest compatible stable versions.

### 2026-06-11 final compatible lockfile refresh

- Ran a final `cargo update` after the local `/dp` fixes and workspace test corrections.
- Compatible lockfile updates included `block-buffer` `0.12.0` -> `0.12.1`, `insta` `1.47.2` -> `1.48.0`, `memchr` `2.8.1` -> `2.8.2`, and `smallvec` `1.15.1` -> `1.15.2`.
- Remaining known newer versions are constrained by dependency ranges rather than local pins: `generic-array` `0.14.7` (latest `0.14.9`) and `shlex` `1.3.0` (latest `2.0.1`).

## Verification

- `cargo fmt --check`: passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --workspace`: passed.
- `cargo test --workspace --all-features`: passed, including doctests.

## Failed

- None in the dependency update, format, compile, lint, default test, or all-features test pass.

## Needs Attention

- `cargo outdated` cannot inspect this workspace directly because it copies the manifest to a temporary directory where relative `/dp` patches like `../asupersync` resolve to missing paths such as `/tmp/asupersync`. I am using `cargo update --dry-run --verbose`, `cargo metadata`, and direct local manifest checks instead.
- All workspace crates currently set `publish = false`. `.github/workflows/publish.yml` is a manual publishability preflight only and documents that crates.io publication is blocked until unpublished sibling path dependencies are independently published and the workspace publication policy changes.
