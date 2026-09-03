# Changelog

All notable changes to [MCP Agent Mail (Rust)](https://github.com/Dicklesworthstone/mcp_agent_mail_rust) are documented in this file.

Versions marked **[Release]** have published [GitHub Releases](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases) with downloadable binaries. Versions marked **[Tag only]** exist as git tags but were never published as GitHub Releases.

Release sequencing now lives in [docs/RELEASE_TRAIN_PLAN.md](docs/RELEASE_TRAIN_PLAN.md), and per-release sign-off packets should start from [docs/RELEASE_READINESS_TEMPLATE.md](docs/RELEASE_READINESS_TEMPLATE.md).

---

## [Unreleased]

### Known issues

- **The published container image is still frozen at `v0.3.13`.** The v0.3.31
  notes said the ghcr image was unstuck; the registry disagrees. `docker.yml`
  has never completed a successful run and has not been triggered since the
  v0.3.29 tag (releases are cut by the maintainer's local release tooling,
  which does not build the image). Until an image is published by the same
  tooling, install from the release archives or build from source.

### Security

- **`am self-update` now verifies the signed release manifest (GH#292).** The
  updater used to compare the archive against an unsigned `SHA256SUMS`
  fetched from the same place as the archive, which is weaker than the
  installer's trust model and than "Downloaded and verified" implied. It now
  downloads `SHA256SUMS.minisig` as well and verifies it over the exact
  manifest bytes with a pure-Rust minisign verifier (ed25519 + BLAKE2b, no
  `minisign` executable required) against the maintainer release key embedded
  in the binary (id `1BBD79B28BF718D0`, the key pinned in the installers).
  Only an authenticated manifest is consulted for the archive digest. A
  missing signature, a tampered manifest or trusted comment, an unknown
  signing key, or a checksum mismatch aborts the update before any binary is
  replaced. The trusted key set is a list so a future key rotation can trust
  old and new keys across a transition. Debug builds accept
  `AM_SELF_UPDATE_MINISIGN_PUBKEY` for the mocked E2E suite; release builds
  refuse to run the updater when that variable is set.

### Added

- **Small archive-ahead deltas are applied in place instead of rebuilding the
  mailbox (GH#284).** When the Git archive is a few messages ahead of a
  canonically healthy database, startup used to reconstruct the entire
  mailbox into a candidate and promote it, which on a large mailbox was a
  full rebuild for a one-row difference and could trip the promotion guard
  for unrelated reasons (GH#271). The pool's archive-ahead reconcile and the
  cli startup self-heal now first ingest only the missing archive messages
  (with any project or agent rows they need) through the runtime engine in
  one transaction on the live file, reusing the reconstruct's importers and
  its canonical-id free-slot checks. The path refuses without writing when
  the delta is not the simple case: more than
  `AM_ARCHIVE_DELTA_APPLY_MAX_MESSAGES` missing messages (default 64, `0`
  disables), an archive file that does not parse, or a canonical id already
  held by a different live message; those cases still reconstruct.
- **`resource://tooling/recent/{window_seconds}` returns real activity.** The
  server's tool dispatch wrapper now records every finished call (tool,
  `project_key`, `agent_name`, latency, ok/error/rejected) into a bounded
  in-memory ring of the last 512 calls, and the resource reads that ring:
  `?agent=` and `?project=` filter by exact argument value, `?limit=` caps the
  entries (default 100), `format=json` stays a no-op, anything else is
  refused with `InvalidParams`, and a window outside 1..=604800 seconds is
  refused instead of silently returning nothing. The ring is per process and
  not persisted across restarts. Previously the resource parsed its
  parameters and always returned `count: 0` (br-ciwph). The Python-parity
  fixture case now pins the filter semantics (an unknown agent yields no
  entries) instead of the old always-empty result.
- **The seven `?{query}` aliases of the static tooling and config resources
  refuse unsupported parameters.** `resource://config/environment`,
  `tooling/directory`, `tooling/schemas`, `tooling/metrics`,
  `tooling/metrics_core`, `tooling/diagnostics`, and `tooling/locks` used to
  parse the query and discard it. They now accept only the documented
  `format=json` and return `InvalidParams` naming the offending key for
  anything else, so a caller passing `?project=` learns it did nothing.
  `resource://tooling/schemas?cluster=<name>` is the one real filter: it
  narrows the schema map to that cluster and refuses unknown cluster names
  with the known list. `tooling/metrics` and `tooling/metrics_core` keep
  accepting `?window=<seconds>` (their counters are cumulative since process
  start) and now report it back as `window_seconds` with
  `window_applied: false` and a note pointing at `tooling/recent`, instead
  of dropping it silently.

- **`am robot handoff --max-seconds <n>` (default 20) with honest partial
  output.** The dashboard used to run one `messages LEFT JOIN
  message_recipients` aggregate per in-progress bead; FrankenSQLite executes
  that join as a whole-table nested loop, so on a 40k-message mailbox the
  command produced no output inside a 600 s timeout. The mail state is now
  gathered in two batched, join-free `IN (...)` lookups with the exact
  `LEFT JOIN` semantics reproduced in Rust (proven against the old SQL as an
  oracle), the `.beads/issues.jsonl` parse skips non-`in_progress` records
  before decoding them, and a wall-clock budget stops the enrichment loop
  with `summary.scanned` / `summary.truncated_by_budget` plus a warn alert
  instead of hanging. Measured on the reference mailbox (16 projects, 697
  agents, 40,422 messages, 66 in-progress beads): 0.30 s end to end.
- **`am doctor capabilities --json` and `am doctor triage --json` are
  accepted.** Both outputs were already JSON, but the flag the agent handbook,
  AGENTS.md, and `triage.capabilities_url` all advertised was a usage error.
- **Real MCP descriptions for `install_precommit_guard` and
  `uninstall_precommit_guard`.** Both tools shipped with an empty
  `description`, so every MCP client listed them without any guidance.
- **The doc-drift guard now pins the `am doctor` verb count, the `am robot`
  subcommand count, and the TUI theme count** straight from the clap tree and
  `NAMED_THEME_COUNT`, so README/AGENTS tables can no longer drift from the
  shipped CLI surface the way they had (8 documented doctor verbs of 28,
  robot counted as 16/17/18, 5 themes of 42).

- **`am doctor vacuum` — supported in-DB reclaim for orphaned pages (GH#289).**
  An archive reconstruct could leave the promoted database with a majority of
  its pages orphaned (`Page N: never used`, `freelist_count=0`) — pure
  space-accounting waste with no supported way to reclaim it, so a successful
  recovery ended in a permanently unhealthy-looking mailbox. The new verb runs
  the same supervised-owner protocol as `repair` (drain first, `--take-ownership`
  for provably dead owners, `--allow-live-owner` escape hatch), VACUUMs and
  ANALYZEs the mailbox in place, and reports before/after page counts and the
  typed integrity class. `--dry-run` previews read-only (allowed while a live
  owner holds the mailbox); structural damage refuses the vacuum and routes to
  `reconstruct` instead.

- **Typed integrity classes in doctor health/triage (GH#286).** `am doctor
  health` and `triage` reported one identical P0 "needs reconstruct" verdict
  for a database with only leaked/orphaned pages (every b-tree and index
  intact, every row readable) and for genuine structural damage, so alert
  rules were either permanently noisy or blind. Integrity-check failures now
  classify as `leaked_pages_only` / `index_only` / `structural` (with
  `leaked_pages`, `structural_errors`, and `first_structural_error` fields on
  the triage finding). Leaked-pages-only reports as a distinct **degraded**
  P2 finding (`live-mailbox-leaked-pages`) recommending `am doctor vacuum`,
  and `am doctor health` exits 0 for it; structural damage keeps the P0
  reconstruct verdict, now annotated with the class counts.

### Fixed

- **Canonical schema init no longer fails on a pre-v27/v28 database.** The
  CLI ran the whole base DDL blob before migrations, so on a Python-era or
  pre-v27/v28 mailbox it created `idx_messages_project_topic` and
  `idx_agents_project_active` before the v27/v28 migrations could add
  `messages.topic` and `agents.retired_at`, and stopped with "no such
  column". The migration ledger already defers those two derived index
  statements behind their column migrations; canonical init now executes
  the base DDL without them
  (`schema::init_schema_sql_base_deferring_column_dependent_indexes`) and
  the deferred migrations create the indexes once the columns exist. The
  base DDL and the recorded migration ids are unchanged.
- **The CLI's explicit project lookup no longer attaches a distinct path to
  a slug-colliding project.** The database human-key lookup gained a
  stable-slug fallback for absolute keys (so reads find the row
  `ensure_project` would reuse), which silently defeated the identity guard
  in `get_project_record`: `am products link --project /x/repo/a/b` resolved
  the registered `/x/repo/a-b`, and a case variant of a registered path
  resolved on a case-sensitive filesystem. The lookup is now layered
  (`queries::get_project_by_human_key_exact` without the fallback, and
  `get_project_by_human_key` with it); the CLI's explicit lookup uses the
  exact layer.
- **Doctor salvage considers a backup the runtime engine wrote.** Salvage
  candidate discovery reused the restore rule that requires a standalone
  main file, which excluded a `.bak` carrying its FrankenSQLite namespace
  pair; the current database's failure was then reported as the only
  candidate. Salvage reads through a private canonical copy, so only
  journal/WAL companions disqualify a candidate now
  (`pool::sqlite_salvage_read_candidates`).
- **Archive save and share export read a family whose live open is refused
  for a damaged sidecar.** When the guarded live open refused a mailbox for
  a recovery-classified sidecar (truncated or header-only WAL, stale SHM) and
  no archive authority existed, the command failed. The snapshot source now
  falls back to a private no-follow copy of the whole family, settled and
  read through canonical SQLite; the live family is never opened, written,
  or cleaned.
- **Backup restore lands again when a sidecar slot holds junk.** A directory,
  device, FIFO, or symlink squatting in the live family's `-wal`, `-shm`,
  `-journal`, or namespace slot made `am doctor restore` (and every other
  recovery promotion) fail with "source is missing or its SQLite family
  contains a non-regular object": the recovery receipt stages the source
  generation before promotion and is deliberately fail-closed on a
  non-regular family member. Promotion now retires such objects by rename
  (the usual `<sidecar>.corrupt-<timestamp>` quarantine name, never deleted)
  before the receipt looks, while regular sidecars stay in place for the
  receipt to account for.
- **The pre-commit guard no longer honors a superseded foreign-generation
  reservation artifact (GH#299).** After a mailbox rebuild the archive can hold
  `id-<id>-g<old>.json` with `released_ts: null` from the previous database
  generation next to `id-<id>-g<current>.json` released under the current one.
  The guard treated the old artifact as an active reservation and refused
  unrelated commits until its `expires_ts` passed (100+ ids on one recovered
  archive). A same-id released artifact from another generation that is at
  least as recent now supersedes the unreleased twin; an old-generation
  artifact with no released twin stays active. `am doctor archive-normalize`
  already quarantines foreign-generation artifacts when it can read the
  live database's generation; when that diagnostic open is refused or the
  database carries no generation it now says so (a warning and a
  `reservation_artifact_detection_skipped` list in `--json`) instead of
  reporting zero reservation actions as if there were nothing to do.
- **Doctor's file-sanity failure says which probe failed (GH#300).** The
  staged health probes answered "unhealthy" without a reason, so
  `am doctor check` reported an opaque `possible corruption` and recommended
  stop-and-reconstruct. Each probe branch now records its reason (the primary
  engine's leading integrity rows, the canonical compatibility probe's error,
  a failed open, a missing or non-regular family member), the staged probe
  re-keys it under the live path and marks it as a private-copy probe, and the
  doctor detail includes it with a pointer to confirm with `am doctor health`
  before reconstructing. `health_check` also lists every input behind its
  `health_level` (`health_level_contributors`: verdict roll-up, live
  pressure, archive lag, coalescer tail latency, deleted executable), so a
  red top level that no decomposed verdict explains is attributable instead
  of looking like flapping aggregation.
- **Auto-registered recipients get an archived profile, and the
  `send_message` description says so (GH#301).** A send to an unregistered
  recipient in the same project auto-registers a placeholder agent
  (program/model `unknown`) when `MESSAGING_AUTO_REGISTER_RECIPIENTS` is on and
  the registration proof gate is off. The placeholder was DB-only, so DB and
  archive agent inventories drifted by one and a reconstruct could not recreate
  it. The same profile shape `register_agent` archives is now written for it,
  and the tool description no longer claims unknown recipients fail fast.
- **`am` reads no longer refuse a mailbox that lacks FrankenSQLite namespace
  sidecars (br-vhxdc).** The CLI's live read-only opener (startup banner,
  pre-open health probe, sync mailbox reads, doctor source selection, robot
  mailbox and attachment reads, migration format detection, agent-start
  reservation checks) now goes through the engine-dispatching guarded opener:
  FrankenSQLite when the family carries its namespace pair, canonical
  SQLite's true read-only flags otherwise. A mailbox last written by canonical
  SQLite, restored from a `.bak`, or produced by archive reconstruction was
  previously refused with "a complete pre-existing namespace sidecar pair is
  required". The schema, health, project, and timestamp-format probes on
  those paths are generic over the connection engine.
- **Database-path policy sees the configured spelling again.** Since the
  mailbox-identity work, the shared path helpers the cli and the server
  used for opening and validating the database returned the frozen identity
  (symlinks followed, relative spellings anchored to the current directory),
  so the startup probes' "must not be a symlink" checks and the cli's
  regular-file checks were validating the resolved target instead of what
  the operator configured, and explicit relative spellings were rewritten.
  `ResolvedMailboxSqlitePath` now carries `selected_path` (spelling after
  only the legacy absolute fallback) beside `canonical_path` (identity), and
  the cli runtime resolver and the server URL resolver use the spelling.
  Registry keys still use the identity. The pool likewise freezes the
  configured storage-root spelling beside its identity and hands the
  spelling to archive recovery and reconcile, so a symlinked storage root is
  refused as archive authority again instead of being followed to wherever
  the link points.
- **`DATABASE_POOL_TIMEOUT` means seconds everywhere (GH#245 follow-up).** The
  tool-call pools parsed the variable as milliseconds while the server
  readiness path and the operator runbook treated it as seconds, so
  `DATABASE_POOL_TIMEOUT=30` gave every tool-call pool a 30 ms acquire wait.
  Both paths now share one interpretation: seconds, with values of 1000 or
  more read as milliseconds so a setting that followed the old code comment
  keeps its meaning. The runbook default is corrected to the shipped 10 s.
- **`am doctor` reads a resting WAL family without touching it (br-s9d8a).**
  A database left with WAL/SHM sidecars by a canonical or Python-era writer
  and no live owner could not be read by the guarded read-only canonical
  opener: WAL recovery needs a writable SHM, the read-only SHM policy turned
  the open into SQLITE_CANTOPEN, and because the open was lazy the failure
  surfaced only on the first query, so no fallback ever ran. `am doctor
  health` reported a healthy mailbox as "Database open probe failed" and
  the fixers' read-only probes declined. The opener now probes eagerly so a
  refusal is an open error; a family with no WAL and no SHM is opened
  `immutable=1` (no locks, no sidecars created beside it); and both the
  doctor's canonical source selection and the explicit-offline candidate
  fall back to the same private staged copy the health probe uses, which
  applies the WAL inside the copy and leaves the source bytes alone.
- **Startup self-heal can diagnose index-only corruption again instead of
  reconstructing from the archive.** Recovery admission arms the durable
  circuit breaker with a provisional failure before an attempt runs. The
  FrankenSQLite guarded opener already let the attempt's own read-only
  probes pass through that arming, but the offline canonical opener did not,
  so every canonical probe issued from inside the admitted attempt (the
  index-only REINDEX classifier and the double-probe cross-check) was refused
  as "recovery-breaker state records 1 failed attempt(s)". The in-place
  REINDEX fast path (br-mdfpz, the fix for the nine-day crash loop on the
  csd host) therefore never fired under startup self-heal and every
  index-only case fell through to archive reconstruction. The canonical
  opener now consults the same admission guard; outside the attempt the
  armed breaker still refuses.
- **`AM_RECOVERY_BREAKER_MAX_CONSECUTIVE_FAILURES` and
  `AM_RECOVERY_BREAKER_COOLDOWN_SECS` are read through the same process-env
  layer as every other `AM_*` knob.** They were read with a raw
  `std::env::var`, so values supplied through the configuration override
  layer were ignored.
- **`sqlite:` URL contract written down and the tool-metrics worker follows
  it (br-z73au).** The three-slash form names an absolute path, the same as
  four slashes; a working-directory-relative database must be spelled
  `sqlite:///./x.sqlite3` or `sqlite://x.sqlite3`. A missing relative target
  is never replaced by an absolute file that shares its suffix; only a
  relative file that exists but is unhealthy next to a healthy `/<same>`
  triggers the legacy absolute fallback. The contract is documented on the
  parser and in the operator runbook. The tool-metrics persistence worker
  used to be tested against the opposite expectation (adopt the absolute
  file); it persists into the runtime's database, and its test now says so.
  Seven fixtures that spelled relative targets with three slashes now use
  the explicit form, and two search-side tests are renamed to describe the
  behaviour they actually pin.
- **Startup no longer pays the staged FrankenSQLite health check three times
  (br-eru3j).** Every staged health probe copies the SQLite family into a
  private tempdir and runs the FrankenSQLite quick check on the copy; on a
  163 MB mailbox that check alone takes 8-13 s under fsqlite 0.3.14, and a
  server start ran it for the startup integrity probe, the pool-init probe,
  and the first `health_check` within seconds of each other (measured
  `initialize` 16-19 s, first tool call 8 s). A healthy verdict is now kept
  for 30 s keyed by the family's metadata (main file device/inode/length/
  mtime, WAL/journal/cert length+mtime, shm and namespace sidecar
  presence+length) and reused while nothing changed, so the check runs once
  per start. Any write invalidates it, unhealthy verdicts are never kept, and
  `AM_HEALTH_VERDICT_REUSE_SECS=0` disables the reuse (max 600). The
  per-validation-unit thread storm inside fsqlite's quick check itself is an
  upstream defect and remains.
- **A healthy live database keeps serving when its archive-ahead reconcile
  fails (br-bgwj1, br-plksu).** Startup and pool initialization reconcile a
  healthy primary against an archive that is ahead of it by rebuilding a
  candidate from the archive and promoting it. When that reconstruction or
  promotion failed (a recovery-receipt refusal over duplicate reservation
  artifacts, a writer that would not drain, an unwritable directory), the
  error propagated out of the integrity probe and the process exited 1 into
  a systemd restart loop, even though the live database had just passed its
  health check. The failure is now caught at the reconstruction step: if the
  primary is still healthy afterwards the server keeps serving it, the drift
  is recorded as pending for the periodic retry and `am doctor health`, and a
  warning names the retry path (`am doctor reconstruct --yes`). Only a
  primary that is no longer healthy after the attempt still fails startup.
  Policy refusals that run before the reconstruction (read-only intent,
  symlinked paths, a tripped breaker, a live owner) are unchanged and stay
  fatal.
- **A failed archive-drift reconcile no longer arms the recovery breaker
  (br-plksu).** The drift reconcile of a healthy primary now runs under the
  mutation-only admission (it still refuses to run concurrently with a real
  recovery) instead of the outcome-recording admission, so its failures
  cannot count toward tripping the breaker against a database that was never
  unhealthy. Regression tests cover both behaviours.

- **Read-only diagnostics can open a database that was never
  Franken-admitted.** Every read-only consumer (mailbox inventory, the
  startup consistency probe, the integrity and schema-population verdicts,
  the archive verifier, the reconstruction message-id inventory, the live
  salvage export, `am doctor vacuum` statistics and the doctor integrity
  classification probe) called the bound FrankenSQLite opener directly,
  which requires the `-fsqlite-ns-gate`/`-fsqlite-ns-use` pair. A family
  last written by canonical SQLite, restored from a `.bak`, or produced by
  archive reconstruction has no such pair, so every one of those paths
  failed with "a complete pre-existing namespace sidecar pair is required"
  and, in particular, archive-ahead recovery of a reconstructed primary could
  never reconcile. `pool::open_guarded_read_only_sqlite_file` now inspects
  the namespace pair once and dispatches: complete pair → the same
  FrankenSQLite opener, no pair → canonical SQLite's true read-only flags, a
  half pair → an explicit refusal. Neither engine's own refusal paths are
  bypassed, and a family that changes engines mid-admission gets exactly one
  re-dispatch. The live-salvage materializer and the proactive-backup export
  materialize a sidecar-less source through canonical SQLite (source bytes
  untouched) instead of refusing it.
- **A restored or reconstructed primary no longer carries the replaced
  generation's FrankenSQLite namespace records.** Promotion quarantined the
  old journal/WAL/SHM companions but left `-fsqlite-ns-gate`/`-fsqlite-ns-use`
  beside the new bytes, so the strict read-only opener refused the promoted
  family ("namespace record names a different database generation") and the
  post-restore archive reconcile failed. The namespace pair is now retired by
  rename with the quarantined generation (never unlinked); the runtime's next
  writer-capable open re-admits the family. A salvage source whose main file
  is shorter than the SQLite header is classified as corruption, so recovery
  degrades to an archive-only rebuild instead of blocking on it.
- **`am doctor` chooses its diagnostic source by namespace authority, and
  `am doctor reconstruct` can salvage the current database.** A family
  without a FrankenSQLite namespace pair is opened directly by the offline
  canonical opener (a physical proof); only a Franken-admitted family goes
  through the guarded logical snapshot, whose verdict is by design at most
  "inconclusive" about the physical b-tree. The doctor's private `VACUUM
  INTO` snapshot is neutralized (namespace records retired, WAL folded)
  before the private-salvage validators see it, and the full-integrity
  validator checks a Franken-admitted quarantined artifact through
  FrankenSQLite instead of refusing a cross-engine open. The HTTP readiness
  probe, the integrity-guard cross-count and the tool-metrics observer use
  the same dispatching opener, so readiness after an archive-backed reconcile
  no longer fails with the namespace-pair refusal. A symlinked storage root
  is never archive authority for the pre-init reconcile, matching the
  startup probe and reconstruct.
- **The Config-level default-archive guard no longer panics under
  cargo-nextest.** The refusal added for br-99aih fired whenever a test
  merely constructed a `Config` from the ambient env under a harness marker,
  which under nextest (the release gate, which exports `NEXTEST_RUN_ID` into
  every test process) failed hundreds of unit tests that never touch the
  archive. `Config::from_env` is back to a warning with guidance
  (`AM_STRICT_HOME_STORAGE_GUARD=1` upgrades it to a panic); the fail-closed
  protection is the storage crate's archive funnel, which still refuses to
  initialize the operator's real archive from any test harness.
- **`am doctor health` / `triage` no longer report a false P0 "cannot open
  database, reconstruct" on a healthy live mailbox.** The doctor's canonical
  diagnostic source had two branches: a `VACUUM INTO` snapshot through the
  read-only FrankenSQLite connection, and a cross-engine canonical open that
  is refused outright for any Franken-admitted family (the `-fsqlite-ns-*`
  sidecars every live mailbox carries). When the snapshot failed on a hot WAL
  the composite error was truncated to 160 characters and the verdict was
  "reconstruct". The doctor now has a third source — the same byte-neutral
  staged family copy `am robot health` already used for `db_file_sanity`,
  opened read-write on the private copy so WAL frames recover — and the
  refusal detail keeps both underlying causes readable. Verified on the
  reference host: the P0 vanished and the doctor reported the mailbox's real
  reservation-parity drift instead.
- **Recovery no longer poisons itself (br-plksu).** Automatic recovery
  admission arms the durable recovery breaker with a provisional failure
  record before running the attempt; the attempt's own live-salvage
  read-only open then refused itself because the breaker was "nonclean",
  recorded a real failure, and readiness crash-looped a healthy database
  until an operator hand-cleared the sidecar. The Franken read-only preflight
  now recognizes an open issued from inside its own admitted recovery
  attempt and proceeds (unrelated readers are still refused).
- **`am doctor drain` / `locks` no longer count a transient read-only CLI
  reader as a second mailbox owner.** Ownership is now decided by an
  exclusive activity lock or a `serve-*` command line; processes that merely
  have the database file open are reported as readers. Previously a
  concurrent `am robot handoff` flipped the verdict to split-brain /
  unsafe-to-touch and blocked every supervised repair.
- **Workspace tests can no longer write into the live default mailbox
  archive (br-99aih).** Thirteen tests overrode only `XDG_DATA_HOME` while
  the legacy `$HOME/.mcp_agent_mail_git_mailbox_repo` branch wins whenever it
  exists, so on any host that has run the daemon they wrote junk projects
  into production and later triggered an archive-ahead reconcile. The
  storage crate's archive-root funnel (every archive write passes through
  it) now refuses to initialize the default root from any cargo / nextest /
  insta harness (escape hatch `AM_ALLOW_HOME_STORAGE_ROOT=1`),
  `Config::from_env` warns with guidance under the same predicate, a shared
  `with_isolated_default_storage_root_for_test` helper redirects
  `HOME`/`XDG_DATA_HOME`, and the leaking tests use it.
- **`am robot metrics` no longer prints zeros from the CLI's own process
  counters.** It reads `resource://tooling/metrics` from the running server
  (which now carries per-tool latency) and labels the payload
  `source: "live-server"`; without a reachable server it reports
  `source: "local-process"` with an explicit alert instead of fabricated
  numbers.
- **Archive-ahead recovery could never promote a live-salvaged candidate.**
  `materialize_live_franken_salvage` ran `VACUUM INTO` through the guarded
  FrankenSQLite source connection, which left `-fsqlite-ns-gate` /
  `-fsqlite-ns-use` namespace sidecars beside the process-private artifact;
  every downstream validator opens that artifact through the guarded
  canonical opener, which refuses any Franken-admitted path outright, so the
  salvage always failed validation with `refusing an archive-only candidate
  because DB-only coordination state could be lost`. The residue is now
  retired (renamed, never deleted) inside the private snapshot directory
  before validation. This is the single root cause behind ~23 deterministic
  test failures on main (the `*_uses_archive_snapshot_when_live_db_is_stale`
  and doctor-reconstruct salvage families).
- **Recovery receipts no longer wedge on a source that cannot even be
  staged.** When the primary is not a readable SQLite file — exactly the
  shape recovery runs for — staging the source-neutral family copy fails with
  a corruption verdict (`file is not a database`), and
  `collect_recovery_receipt_evidence` turned that into a hard
  `source generation health staging` refusal, so no backup or archive
  candidate could ever promote. A corruption-class staging failure now
  attests the source as unverified (the same outcome an unreadable source
  already got when staging succeeded); environmental failures (ENOSPC,
  EPERM, a non-regular family member) stay fail-closed. Root cause of ~20
  deterministic `restore/staging/reconstruct` test failures on main.
- **`search_index_generation.db_identity` no longer leaks the internal cache
  namespace** (`utf8:<path>@<gen>`); operator surfaces show
  `<path>@<generation>` while process-global caches keep the discriminator.
- **Tool input-schema parity understands nullable type arrays.** fastmcp
  0.7.1 deliberately publishes `Option<T>` parameters as
  `{"type": ["T", "null"]}` (GH#255 Python parity, CHANGELOG v0.3.31); the
  conformance comparator only read `type` as a string and reported 108
  "lost" parameter types across 29 tools. The comparator now normalizes both
  nullable spellings to the base type and still fails a property with no
  non-null type at all. The Python fixture is untouched.
- **The `mark_all_read` landing is finished:** the CLI mode matrix, the
  conformance audit baseline (45 tools = 38 compatibility + 7 Rust-native),
  the audit document table, and the error-code catalog (`AGENT_DEREGISTERED`,
  `AGENT_RETIRED`, `AUTHENTICATION_REQUIRED`, `INVALID_TOPIC`, all shipped
  in earlier releases) now agree with the live inventory.
- **Source builds pin the frankensearch sibling to the release commit.** The
  live `../frankensearch` checkout moved to asupersync 0.4.10
  (`Cx::is_cancelled` in frankensearch-quill) while fastmcp — including 0.8.1
  — still pins asupersync =0.4.9, which made the whole workspace uncompilable
  from a live sibling. `Cargo.toml`, `dist.yml`, `Dockerfile`, and
  `install.sh --from-source` now use a gated `../frankensearch-rel-0332`
  checkout at `FRANKENSEARCH_COMMIT`.
- **`deploy-pages.yml` no longer watches a leaked rch temp path**; it is
  dispatch-only with a `bundle_dir` input.
- **Docs match the binary:** AGENTS.md's doctor examples used flags the
  binary rejects (`am doctor --fix`, `--dry-run --fix`, `--json`); its
  dependency table named crates that are not in the workspace and a
  `DATABASE_URL` default that is not the default; README's family table
  listed 8 of 28 doctor verbs, 17 of 19 robot subcommands, and 5 of 42
  themes, and claimed hybrid search for release binaries that ship the
  lexical tier only. VISION.md carries dated reality notes for the two
  claims the code contradicts (C SQLite is statically bundled for
  verification cross-checks; tool-prose parity is Rust-owned, not gated).

- **Doctor advice consults the recovery breaker before recommending
  reconstruct (GH#287).** `triage` recommended `am doctor reconstruct
  --dry-run` while the recovery-breaker sidecar `am` itself wrote recorded
  that reconstruct fails deterministically on this mailbox. The reconstruct
  finding (and its planned action) now carries `blocked`, `blocked_reason`,
  and `blocked_since` from the breaker record, and `am doctor health` prints
  the recorded failure next to the advice, so operators and agents stop
  looping on a known-failing command.

- **Integrity guard fingerprints standing defects and backs off repeat WARNs
  (GH#288).** The background guard re-warned the identical defect on every
  cycle (~23x/day for days) with no dedup, saturating journal-based alerting.
  Both the guard's detection WARN and the pool's "integrity probe and
  canonical SQLite both rejected the file" WARN now log the transition once,
  a "still standing since <ts>, N observations" reminder on a 6-hourly
  cadence, and everything in between at debug; a fingerprint change or a
  passing cycle re-arms the fresh WARN. When the recovery breaker records
  that the promised reconstruct is blocked, the guard message now says
  "recovery unavailable: …" with the recorded reason instead of promising a
  recovery that never runs.

## [v0.3.32](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.32) — 2026-09-01 **[Release]**

Recovery-operability release: the promotion guards learned to explain
themselves and to offer supported ways out, closing the operator dead-ends
reported in GH#271, GH#283, GH#284, and GH#285. The reservation stable-key
promotion fix was verified live: it promoted a real 2.7 GB cross-linked
production mailbox (16 projects / 658 agents / 40,386 messages, 0 parse
errors) that v0.3.31 refused.

### Added

- **`am doctor reconstruct --reseed-receipt-chain` (GH#283).** A structurally
  broken recovery-receipt chain (zero or multiple roots, broken link, fork,
  cycle, invalid self-hash) used to deterministically refuse every future
  promotion — including a fully valid archive candidate — with no supported
  way out, forcing operators into manual DB swaps that bypass every guard.
  The new flag (requires `--yes`; `--dry-run` previews the chain verdict
  read-only) quarantines the entire receipts directory by rename — never
  deletion — and lets the next promotion seed a fresh root. Refused when the
  chain verifies cleanly or an unfinalized promotion intent exists.

### Fixed

- **CLI no longer queues durable UNSENT artifacts for definitive server
  refusals (GH#285).** A daemon-proxied tool rejection arrives as the full
  legacy error envelope; the queueing classifier ran substring heuristics
  over that raw JSON, where a suggested recipient name or task description
  could satisfy the WAL-sidecar-corruption pattern — so an
  `INVALID_ARGUMENT` (bad recipient name) was recorded as
  `wal_sidecar_corruption` / `blocks_edits: true` and queued as an UNSENT
  artifact that could never replay successfully. Client-refusal codes
  (`INVALID_*`, `*_NOT_FOUND`, policy/cursor/token refusals) now never
  queue, and server-fault envelopes classify on the tool's own message text
  instead of the envelope payload.
- **Reconstruct promotion refusals now name the colliding reservations
  (GH#271).** "reservations produced N rows but only M unique stable keys"
  now appends the colliding stable keys (project, agent, path, lifecycle
  fields; up to 5 samples) so operators no longer have to inspect the
  refused candidate database by hand.
- **A corrupt source that cannot even be opened no longer vetoes promotion
  of a healthy archive candidate (GH#283 outage family).** The promotion
  gate's full-integrity probe ran against the settled private staging copy
  through a read-only canonical open; when the damaged main-file header
  still demanded WAL recovery, every probe form failed with "unable to open
  database file" and promotion refused because the source could not be
  classified at all. The probe now opens the private throwaway copy
  writable, letting canonical SQLite run recovery and return a real
  corrupt/healthy verdict (the authority path stays byte-untouched).
- **Robot snapshot caches now survive real poller cadences (GH#274).** The
  status/inbox/reservations/overview/agents caches required both a matching
  generation fingerprint AND an age under 500ms, so every poll on a
  seconds-to-minutes cadence paid the full multi-query rebuild even when
  the fingerprint proved nothing had changed. Generation-verified entries
  now live 30s by default (`AM_ROBOT_SNAPSHOT_TTL_MS` overrides); counts
  stay exact because the fingerprint is recomputed on every call.

## [v0.3.31](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.31) — 2026-08-31 **[Release]**

Durability-diagnostics release: the database layer learned to explain itself.
A dedicated corruption-forensics engine, connection-pool lease tracking, and
startup WAL preflight replace the previous "it is broken, good luck" failure
mode with reports that name the page, the lease, and the checkpoint that went
wrong. Also lands first-class Oh My Pi support and verifiable pane-identity
bindings (GH#252), and unsticks the container image, which had been frozen at
v0.3.13 and amd64-only since June (GH#256).

This is also the first release under the new installer trust model: releases
are authenticated by a minisign signature over `SHA256SUMS` made with a
maintainer-held key, replacing the GitHub-Actions Sigstore identity that no
new release could satisfy (GH#269 in the Python repo; acfs#365). See the
Security section below.

### Security

- **Installer trust model: mandatory minisign for v0.3.31 and later.**
  Releases are no longer built by GitHub Actions, so the installers' previous
  requirement — a keyless Sigstore bundle certified for the
  `dist.yml@refs/tags/<tag>` Actions workflow identity — had become
  unsatisfiable: v0.3.30 shipped without bundles and `install.sh` correctly
  failed closed on every host. For releases >= v0.3.31, `install.sh` and
  `install.ps1` instead require (a) the per-archive SHA-256 resolved from the
  release `SHA256SUMS` manifest AND (b) a valid detached minisign signature
  (`SHA256SUMS.minisig`) over the exact manifest bytes, verified against the
  maintainer release key pinned in the script (epoch 2, key id
  `1BBD79B28BF718D0`, shared with the frankensqlite release line). A missing
  `minisign` binary, manifest, signature, or checksum entry aborts the
  install; verification never degrades to checksum-only, and `--no-verify`
  semantics are unchanged. The Sigstore/cosign path is preserved intact for
  installing releases older than v0.3.31 — `cosign` is no longer required for
  current releases (which also sidesteps the unrelated Ubuntu 26.04
  packaged-cosign breakage). The trust anchor moved from "GitHub's CI
  identity for this repository" to "a signing key the maintainer controls";
  the model stays fail-closed. Details in `SECURITY.md`.

### Added

- **First-class agent lifecycle tools (GH#255).** The MCP surface now exposes
  `retire_agent`, `unretire_agent`, and `deregister_agent` with registration-token
  or verified-pane authorization. Retirement is reversible; deregistration is
  permanent for that identity. Both preserve message, reservation, and archive
  history, remove the identity from active rosters and new-message routing, and
  survive archive reconstruction, snapshot/export, legacy import, and restart.
  Retirement retries preserve the first durable timestamp atomically, including
  when requests race, until an explicit unretire transition occurs.

- **Durable message topics and project topic search (GH#259).** `send_message`
  now accepts a validated optional topic, replies inherit their parent's topic,
  and topic metadata survives SQLite migration, archive writes, reconstruction,
  snapshots, CLI/robot output, and TUI rendering. `fetch_inbox(topic=...)`
  performs exact case-insensitive recipient filtering, while the newly registered
  `fetch_topic` compatibility tool searches the whole project without allowing
  unrelated newer mail to displace matching rows before the result limit.

- **First-class Oh My Pi (OMP) support.** Agent detection now recognizes the
  `omp` connector and `oh-my-pi` alias, while setup writes OMP's native
  authenticated HTTP MCP shape to the project config and active-profile user
  config. Discovery, doctor repair, and the installer honor `OMP_PROFILE`,
  `PI_PROFILE`, `PI_CONFIG_DIR`, and default-profile `PI_CODING_AGENT_DIR`,
  enforce OMP's lowercase profile-name syntax, cover existing named profiles
  plus OMP's project-root fallbacks, and avoid following symlinked or invalid
  profile directories. Installer migration removes stale stdio/auth fields and
  canonicalizes OMP entries under `mcpServers`; every installer phase now
  shares the same bearer token. Pane identity parsing also treats `omp` and
  `oh-my-pi` as program names rather than agent names.

- **Verifiable pane-identity bindings (GH#252).** Per-pane identity files
  now hold a one-line JSON `PaneIdentityRecord` — `name` plus the tmux
  binding facts `session_name`, `pane_id`, `pane_pid`, `socket_path`,
  `written_at` — written through the same symlink-hardened path as before;
  legacy bare-name files still parse (as a record with only `name`), and
  unknown fields are tolerated on read. A binding is **live** iff
  `tmux -S <socket_path> display-message -t <pane_id>` reports the recorded
  `session_name` and `pane_pid` and a non-shell `pane_current_command`;
  any failing check, a gone server, or a missing socket file is **dead**.
  Resolution (`resolve_pane_identity`, `am agents resolve-pane`,
  `macro_start_session` reuse, the `X-Tmux-Pane` header path, the git
  guard) applies the adoption rule to every candidate in the existing
  lookup order: a live binding held by a *different* pane is never handed
  out (the lookup reports not-found so the caller mints a fresh name), a
  dead binding is adopted and its record atomically rewritten with the
  adopter's facts, and legacy/unverifiable files resolve under a
  conservative compatibility rule (kept as-is while the key pane runs an
  agent, adopted and upgraded to a structured record when it idles in a
  shell, returned untouched with no tmux context). Writers
  (`register_agent`, `create_agent_identity`, `write_identity`) populate
  the new fields and refuse to overwrite a verifiably live binding held by
  another pane (best-effort warn-and-continue for callers). The
  `resolve_pane_identity` tool response and `am agents resolve-pane` gain a
  `binding` field: `verified-live`, `adopted-dead`, or `legacy-unverified`.
  `cleanup_pane_identities` judges structured records by the same
  predicate — live bindings are never removed, dead ones are purged — while
  legacy files keep the live-pane-list rule; a process that cannot execute
  `tmux` treats structured records as unverifiable, and socket-gone records
  are purged only when tmux reports live panes on this host, so a stopped
  or unreachable tmux can never mass-purge identities.

- **Database corruption forensics engine.**
  `crates/mcp-agent-mail-db/src/forensics.rs` performs corruption detection and
  WAL inspection and writes structured reports under
  `<storage_root>/doctor/forensics/`, which `am doctor` surfaces by path so an
  operator (or an agent) can attach the evidence to a bug report instead of
  reconstructing it from logs.

- **Connection-pool lease tracking and acquisition metrics.** The pool now
  records per-lease ownership and timeout recovery, so a connection leak
  reports which lease outlived its budget rather than presenting as a generic
  acquisition timeout. Doctor diagnostics and the pool health check read the
  same counters.

- **Startup preflight WAL integrity checks.** Server startup validates WAL
  state before accepting traffic, and the health endpoints expose the result.

- **Backup rotation retention and atomic snapshot export.** Retention pruning
  is validated before it prunes, and snapshot export is atomic with boundary
  metadata recorded alongside the snapshot.

### Fixed

- **`am check-inbox` matches the daemon's view of the inbox (GH#269)** and the
  CLI gains an `am agents reap` verb (GH#275) for reclaiming dead agent
  identities without hand-editing state.

- **Backpressure health is classified from rolling queue-wait windows
  (GH#272)** instead of instantaneous samples, so a single slow acquisition
  can no longer flap the health verdict, and the KPI/metrics surfaces report
  the same windowed classification.

- **Reservation-scan SQL is computed in Rust and guarded against ledger
  anti-join regressions (GH#274).** The TUI poller's release-ledger joins no
  longer depend on SQLite schema variations, the duplicated reservation
  helpers were de-duplicated, and a regression test pins the anti-join shape.

- **`get_project_by_human_key` falls back to the stable slug (GH#267)** when
  the human key lookup misses, so renamed projects resolve consistently.

- **macOS builds compile again**: `rustix::fs::RawMode` is `u16` on Apple
  targets (libc `mode_t`) but `u32` on Linux, and the setup-file permission
  helper passed a `u32` straight through — Apple-target builds failed with
  E0308 after the rustix 1.1.x refresh. The permission bits are now narrowed
  explicitly (always <= `0o7777`, so the cast is lossless).

- **Archive reconstruction no longer lets salvage rows collide with archive
  identities.** Reconstruction and live WAL readiness were hardened, archive
  reservation identity now wins over a salvaged local row id, and
  archive+salvage lease dedup is pinned by test — this disproved the salvage
  hypothesis for the 881/873 reservation-parity field outage.

- **ATC hydration is bounded and fair for large recent populations (GH#258).**
  Liveness evaluation drains at most eight scheduled or policy-dirty agents per
  tick, deduplicates agents represented in both queues, preserves deterministic
  progress across a 940-agent cold start, and advertises an immediately due
  follow-up deadline while dirty work remains. This prevents a valid population
  larger than the 512-effect executor queue from being materialized in one
  multi-second burst. `am robot health` now also derives `health_level` from its
  aggregate probe verdict, so failed live-server probes cannot coexist with a
  misleading green headline; the former capacity-only signal remains available
  as `capacity_health_level`.

- **ATC no longer writes liveness-mail by default (GH#264).** The executor now
  defaults to `shadow` (including for missing or unknown configuration), so
  passive observation remains available but a fresh daemon cannot append
  ordinary `AirTrafficControl` messages or release reservations. Durable
  Canary/Live execution now requires an explicit executor-mode opt-in.

- **Linux release artifacts have a pinned portability floor (GH#262).** Both
  x86_64 and aarch64 GNU builds use pinned `cargo-zigbuild`/Zig tooling with a
  maximum glibc requirement of 2.28, verified from each packaged binary. Every
  release also publishes and executes the static x86_64 musl archive on Ubuntu
  22.04, and the signed release-envelope census fails if any of the six platform
  archives or their checksums are absent.

- **Legacy-import targets are reopenable by a fresh runtime process (GH#268).**
  The importer still performs canonical and FrankenSQLite validation before
  success, and its Linux regression now launches a distinct process to acquire
  the imported target's namespace gate and read the database. The runtime engine
  pin includes the namespace-lifecycle fixes needed for import followed by
  `serve-http` on the same storage volume.

- **Swarm writes now end at a real integrity/restart gate (GH#257).** The
  100-agent message burst and 30-second mixed read/write workload run a full
  integrity check with the writer pool live, drop every pooled connection,
  reopen the same file through a fresh pool, and run full integrity again. This
  gate also verifies independent durable message and recipient row counts on
  both sides of the restart, rather than treating a successful API return or
  integrity pragma alone as proof against lost writes. It is paired with the
  FrankenSQLite 0.3.11 line containing the pager, allocator, and autoindex
  hardening that postdates the affected 0.3.4 release; the historical field
  artifact itself was not reproduced in-tree.

- **The published container image is unstuck (GH#256).** `ghcr.io` had served
  nothing newer than `v0.3.13` since June, and the `latest` index carried a
  single amd64 manifest — arm64 hosts had no image at all. The Dockerfile was
  cloning ten sibling repositories to satisfy `[patch.crates-io]` entries that
  the 2026-08-22 registry adoption had already removed; only two out-of-tree
  `path` dependencies remain (`frankensearch`, `beads_rust`) and the clone list
  now matches them exactly. A drift guard runs after the source clone: it reads
  every `path = "../…"` out of `Cargo.toml` and fails the build naming any
  sibling that was not cloned, so the next time that invariant breaks it breaks
  loudly instead of surfacing as `No such file or directory (os error 2)` deep
  into a cargo build.

- **Installer shares one bearer token across all setup phases**, instead of
  minting a fresh token per phase and leaving earlier phases pointing at a
  credential the server no longer honors.

- **OMP config discovery hardening**: symlink handling and header
  reconciliation no longer follow symlinked or invalid profile directories.

- **Trusted macOS system directory aliases** are accepted in real-directory
  checks, fixing spurious failures on `/tmp` → `/private/tmp` style aliases.

### Changed

- **Rust is now the conformance authority.** The legacy Python behavior fixture
  remains a supported client/migration contract, but its default runner now
  requires the recorded object fields and values rather than whole-object byte
  equality. Additive Rust response fields are accepted; arrays remain exact,
  missing or changed legacy fields still fail, and Rust-native goldens remain
  exact. This prevents the frozen `legacy-python@0.3.0` snapshot from vetoing
  reliability and observability improvements that are backwards-compatible.
  Documented per-field normalization also excludes the legacy health check's
  unconditional `status=ok`; Rust-native tests own the fail-closed health
  contract when durable state is unavailable. Tool and resource prose is now
  Rust-owned; compatibility checks retain supported inventory and input types,
  reject new mandatory inputs, and allow optionalization or clearer wording.

- **`send_message.auto_contact_if_blocked` now accepts explicit JSON `null`** (GH#255 Python parity, final delta): fastmcp 0.7.1 publishes nullable `["boolean", "null"]` schemas for `Option<T>` tool parameters and treats explicit `null` as omitted at extraction, so `null` and omission both take the server-default path (`messaging_auto_handshake_on_block`). The same widening applies to every optional tool parameter this server exposes. Dependency: fastmcp family 0.7.0 → 0.7.1; the pinned dispatch contract test flipped from asserting a loud typed rejection to asserting Python-parity acceptance.

- **`Dockerfile.release` is the new path for published images.** It packages
  the exact binaries that ship in the GitHub release — the ones `dsr`
  cross-builds — into the same runtime stage, so the container and the release
  tarballs are the same bytes, and a genuine amd64 + arm64 index takes minutes
  instead of an hours-long QEMU compile. It self-verifies: both binaries are
  executed inside the image (a wrong-architecture copy fails the build rather
  than shipping a container that dies on first start) and `am --version` must
  match the packaged version. `Dockerfile` remains the build-from-source path
  for building an image from an arbitrary commit.

- Dependency refresh: 48 crates advanced within their existing semver ranges
  (predominantly the `gix` family behind `vergen-gix`, which is build-time
  only). The runtime remains on the `asupersync = 0.4.9` / `sqlmodel = 0.4.0`
  universe, with FrankenSQLite advanced from 0.3.8 to the exact 0.3.11 release.


## [v0.3.30](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.30) — 2026-08-23 **[Release]**

Fleet-wide reliability campaign: root-caused the "97% error rate" display
and the progressive sluggishness that returned within a day of
`am clear-and-reset-everything`, plus a registry-clean dependency universe.

### Added

- **Transcript-safe identity creation (GH#255).**
  `create_agent_identity` now accepts `return_registration_token`
  (default `true`, Python-parity with the Python server's issue #154).
  When `false`, the freshly-minted registration token is omitted from
  the tool result and the response carries
  `registration_token_returned: false` instead; the token is still
  generated and persisted server-side. Unlike the Python server there is
  no per-session identity binding, so an opted-out caller sends with
  `verified_sender: false` unless it obtains the token via an
  operator/admin path — the tool description documents this.
  Also pinned by test: `send_message` with `auto_contact_if_blocked`
  omitted takes the server-default path (Python-parity), while an
  explicit JSON `null` is rejected loudly with a typed `InvalidParams`
  error naming the field (nullable `Option<T>` schemas are a pending
  fastmcp enhancement; the test flips to asserting acceptance once that
  lands).

### Fixed

- **Dashboard "Error %" no longer lies on idle servers.** HTTP request
  counters previously excluded every high-volume success path
  (`/healthz`, `/mail/ws-state`, web-dashboard polls) while still
  counting 401/404/405s, so a mostly-idle server displayed error rates
  approaching 100% (the infamous "97%"). All completed requests now
  count; the summary tile reports server faults (5xx) with 4xx shown
  separately, and the same math is used on every TUI surface.
- **`inbox_stats` recompute is no longer O(recipients × messages) inside
  every write transaction.** The uncorrelated `IN (SELECT … FROM
  messages)` subqueries (which FrankenSQLite does not rewrite into
  semi-joins) became indexed PK joins; `rebuild_all_inbox_stats` is one
  set-based `GROUP BY` aggregate. This was the main "everything slows
  down as the mailbox grows" driver.
- **Integrity guard stopped re-verifying the whole backup and walking the
  entire archive every 5 minutes.** Fresh `.bak` files skip
  re-verification (they are fully validated at creation); the archive
  drift reconcile (quick_check + read/JSON-parse of every message file)
  only retries when a prior drift was actually recorded.
- **`am robot` commands no longer read and parse every archive message
  file on every invocation.** The archive-lag probe uses a cheap file
  count and short-circuits when the local DB is populated and up to
  date — the source of multi-second CLI latency that regrew after resets.
- **A benign "project not found for current directory" no longer
  escalates into a 5.8-second corruption-flavored recovery diagnostic**
  (duplicate `lsof`/`ps` ownership scans, git forensic timeline, full
  verdict). Unregistered directories get a fast, honest info alert;
  genuine DB failures keep the recovery path, now on
  `VerdictOptions::fast()` with the duplicated ownership scan removed.
- **`GET /mail/ws-state?system_health=1` no longer runs an unbounded
  libgit2 ref sweep inline per request** on the 4-worker HTTP runtime
  (the cause of ws-state hangs while `/health` stayed responsive). The
  sweep and its config/dismissal loads are cached per configured
  interval.
- **ATC operator loop can no longer spin at ~1 kHz** when an agent review
  is overdue: waits clamp to the 250 ms tick floor and the sleep is
  computed from a fresh timestamp.
- **Dispatch zombies no longer 503 an idle server forever.** Timed-out
  blocking work that ignores cancellation still counts against admission
  for a bounded window (`AM_DISPATCH_ZOMBIE_ADMISSION_TTL_SECS`, default
  300s) and is then excluded, tracked, and metered
  (`zombies_expired`/`zombies_expired_total`), with both figures named in
  timeout diagnostics.
- **Tool metrics distinguish client refusals from server faults.**
  Invalid-input, not-found, policy/contact, feature-disabled,
  idempotency, cursor-window, and sender-token refusals now increment a
  new per-tool `rejections` counter (surfaced via
  `resource://tooling/metrics`); `errors` counts only server faults, so
  a healthy server with `WORKTREES_ENABLED=false` no longer shows
  `acquire_build_slot` at 100% error. Backpressure sheds record the call
  and a rejection instead of registering nothing.
- **Runtime SQLite `busy_timeout` dropped 60s → 20s**
  (`DB_RUNTIME_BUSY_TIMEOUT_MS`, compile-time-asserted below the 30s
  dispatch deadline) so lock-contended queries yield their thread before
  the dispatch layer zombifies it; one-shot recovery/maintenance paths
  keep 60s deliberately.
- **`column_exists` probes are DQS-safe on fsqlite 0.3.8+** (share
  scrub/scope): an unresolved double-quoted identifier degrades to a
  string literal, so the old `SELECT "col" … LIMIT 0` probe reported
  phantom columns; `PRAGMA table_info` now runs first.
- **Lock-free archive commits adapt to git2 0.21** and heal an
  unloadable HEAD before taking the reference-transaction lock
  (previously GIT_ELOCKED made in-lock healing impossible); a non-UTF8
  symbolic HEAD target now surfaces as an error instead of silently
  detaching via the `HEAD` fallback.

### Changed

- **Pool auto-sizing respects FrankenSQLite's concurrency contract.**
  `auto_pool_size()` now caps at 32 connections (was up to 200;
  min 4–16) — ≥10 concurrent autocommit writers is unsupported upstream
  (fsqlite bd-9inpb, P0) and the swarm-tested bound is N≤32. Explicit
  `DATABASE_POOL_SIZE` overrides are honored unchanged.
- **Dependencies resolve from crates.io** — asupersync `=0.4.9`, fsqlite
  `0.3.8`, sqlmodel `=0.4.0`, fastmcp `0.7.0`, ftui `0.5.0`,
  franken-agent-detection `0.1.10`, tru `0.2.3`. The machine-specific
  `/data/projects/frankensqlite`, `../sqlmodel-rel-0327`,
  `../frankentui`, `../toon_rust`, and `../franken_agent_detection`
  patches are gone (they made source builds fail on any host without
  those exact checkouts). Remaining path patches: `beads_rust`
  (registry pins asupersync `=0.4.4`) and `frankensearch` (registry
  0.3.2 predates the asupersync 0.4 universe).

## [v0.3.29](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.29) — 2026-08-19 **[Release]**

Post-0.3.28 hardening of the registry-engine adoption. The v0.3.28
binaries were cut from a snapshot that predates everything below.

### Fixed

- **`am archive restore` accepts archives containing cross-project
  messages
  ([#251](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/251)).**
  The recovery-receipt ownership joins required a message's sender (and
  a recipient) to belong to the message's own project, so a snapshot
  containing the welcome message that `macros contact-handshake
  --to-project` files under the *recipient's* project could never be
  promoted — every restore of a mailbox with any cross-project
  coordination failed with "recovery receipt message ownership join
  failed" and rolled back. Sender/recipient rows now resolve by agent id
  alone (genuinely missing rows still refuse promotion, and the refusal
  message says which class it found), the mislabeled
  `OrphanMessageSender` schema invariant matches, and a regression test
  pins the handshake shape restoring while true orphans keep failing
  closed.

- **Recovery no longer leaves FrankenSQLite WAL-cert sidecars beside a
  replaced database — ends the non-converging "Page N: never used"
  heal loop
  ([frankensqlite#364](https://github.com/Dicklesworthstone/frankensqlite/issues/364)).**
  The engine's `-wal-cert` / `-wal-cert-head` sidecars carry frame and
  `db_size` state for the specific database file they were written
  beside. Repair/reconstruct promotion rotated only `-journal` / `-wal`
  / `-shm`, so a stale cert survived the file swap and the engine's
  next open replayed it, re-extending the fresh database to the old
  file's exact page count and orphaning the entire gap within seconds
  — which made every heal self-defeating. The cert suffixes now rotate
  with the classic sidecars through every recovery path
  (candidate-conflict checks, live-sidecar probes, post-checkpoint
  quarantine, promotion quarantine, rollback restores), with a
  promotion regression test and doctor-receipt labels for the new
  sidecar kinds. An engine-side ask (bind the cert to the db file's
  identity) is filed upstream as frankensqlite#364.
- **A misspelled or phantom `project_key` answers quickly with the
  "did you mean" message instead of an 11-second stall that the CLI
  reported as a raw `os error 11`.** Two paired defects: (1) every
  project-lookup miss ran the orphaned-project inventory augmentation —
  four anti-join scans over `messages`/`agents`/`file_reservations`/
  `product_project_links` plus per-orphan MIN() aggregates, twice
  (lookup fallback, then fuzzy suggestions) — even though those
  placeholder rows can only ever match the literal
  `[unknown-project-N]` shape; the scans are now gated on that shape
  and the suggestion pass reads plain project rows only. (2) The CLI's
  daemon-proxy tool-call deadline was 10s — just under that server-side
  worst case — and a deadline expiry surfaced the raw transient errno
  (`Resource temporarily unavailable (os error 11)`) instead of a
  timeout message. The deadline is now 30s (matching the local-fallback
  bound) and an exhausted response/send deadline reports a legible
  `request to … timed out after …` error with unchanged
  unavailable-classification semantics.
- **`am doctor mcp-selftest` runs a valid MCP lifecycle
  ([#248](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/248)).**
  The self-test now sends the standard `notifications/initialized`
  notification and a `tools/list` with an explicit empty params object,
  so release qualification exercises the lifecycle real clients use.
  The released v0.3.28 `am` still reports the self-test false negative;
  the fix is diagnostic-only.
- **Auto-handshake no longer converts transient store contention into a
  spurious "Contact approval required" refusal.** `send_message` /
  `reply_message` checked the handshake result with `.is_ok()` alone;
  a busy/recovery/validation blip during the handshake's own writes now
  gets one classified retry, while policy refusals keep their exact
  semantics.
- **fsqlite 0.3.4 contention absorbed with bounded retries** in three
  spots the pre-registry engine never stressed: the pre-transaction
  message-id floor read, pool checkout (BusyRecovery / validation-ping
  failures, 6 attempts with backoff), and inbox reads. Under saturated
  hosts these previously surfaced raw `ResourceBusy` / "connection
  validation failed" errors to tool callers.

### Changed

- **The full test gate is now green on saturated gate hosts: load-lab and
  latency-measurement suites run exclusively, and era-stale latency budgets
  were recalibrated to measured same-host reality.** Three structural fixes:
  (1) the `v3_lexical_*` Tantivy search tests each get a private per-process
  index directory — the old fixed `$TMPDIR` path was guarded by an in-process
  mutex that nextest's process-per-test model cannot see, so concurrent test
  processes raced on the Tantivy writer lock; (2) stress/load/SLO suites
  (`stress_pipeline*`, `load_bench`, `load_concurrency`, `db::stress`, the
  CLI transport-harness gates, PTY/TUI perf tests) are scheduled by nextest
  with `threads-required = "num-test-threads"`, so latency assertions measure
  the code on an effectively idle machine instead of measuring the host
  scheduler under 128-way oversubscription; (3) three hard budgets that were
  calibrated on 2026-02 hardware with the pre-registry engine — the 30-agent
  pipeline p99 (10s → 120s), and the swarm lab's max-operation p95/p99
  (1s/3s → 4s/6s) — were recalibrated to ~2x the measured idle numbers from
  the same-host v0.3.27-vs-main A/B (see benches/BUDGETS.md "Era note
  (2026-08-18)", which records why these were environment-era artifacts and
  not engine regressions).

- **fastmcp resolves from the dedicated gated clone
  `../fastmcp-rel-0328`** (pin history: 206d583 → 2a5ee3b, the fastmcp
  GH#250 rev: unknown legacy methods are `-32601` Method Not Found and
  streamable HTTP survives malformed POST bodies), and the sqlmodel
  patch uses the relative `../sqlmodel-rel-0327` path so dsr buildroots
  resolve it on every host.
- **Gate suites realigned with shipped architecture** (fsqlite 0.3.4 /
  era-aware fastmcp): ATC tests seed and verify through the atc.sqlite3
  sidecar, reconstruct fixtures are identity-strict (br-r6awv
  semantics), export-FTS tests read back through canonical SQLite, MCP
  protocol tests speak the gated lifecycle, and stale CLI/TUI goldens
  were regenerated. Three upstream fsqlite 0.3.4 bugs found during
  gating are filed in the FrankenSQLite tracker (bd-q3hu3, bd-qgh42,
  bd-dhhxp).

### Known issue — stress-suite latency budgets don't hold on current gate hardware (NOT an engine regression)

Settled by a same-host A/B: the v0.3.27 tree (pre-registry engine,
built from the preserved 0.3.27 release buildroot) and current main,
run back-to-back on the same idle 64-core host, both deliver the
30-agent DB+Git pipeline fully correct (150/150, 0 errors) at
statistically identical latency — v0.3.27 p99 = 39.8s, main p99 =
38–47s — against the suite's 10s budget and its checked-in 6.8s
baseline from different-era hardware. The fsqlite 0.3.4 adoption is
NOT the cause (the initially-filed engine bug was retracted and closed
as bd-pr6ii after this measurement); the budgets and baselines are
stale for this host class. The same applies to the three sibling
suites that exceed their 240s watchdogs
(`stress_multi_project_120_agents`, `swarm_load_lab_ci_smoke`,
known-bad-git `scenario_a_clean_baseline`). These four stay red until
the budgets are recalibrated against measured per-host baselines —
deliberately unmasked so the recalibration happens consciously. The
directly-observed fsqlite 0.3.4 error-shape bugs (bd-dhhxp, bd-q3hu3,
bd-qgh42) are independent of this and remain open upstream; the CLI
bench catalog also needs care — `mail_threads` references a removed
verb and the criterion archive bench trips the ephemeral-project guard
(needs `AM_ALLOW_EPHEMERAL_PROJECT_ROOTS=1`).

---

## [v0.3.28](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.28) — 2026-08-16 **[Release]**

Rollup of everything since v0.3.27: the #246 standby-resident takeover
(fleet-critical fix for the restart-loop class), the #247 integrity split,
the #244 reservation-parity fix, the #245 pool-timeout stall fixes, and the
#243 installer service-management fail-safe.

### Fixed

- **Managed duplicates stand by instead of restart-looping forever (#246).**
  When `agent-mail.service` found a verified Agent Mail peer already owning
  the storage root (an interactive `am`, or an orphan outside the unit), it
  logged "managed duplicate exits successfully" and exited 0 — which can
  never converge: `Restart=always` units relaunch any exit unconditionally
  (NRestarts=165 observed in the field), and the shipped `Type=notify` unit
  counts an exit before `READY=1` as a protocol failure, so even
  `Restart=on-failure` looped. The managed duplicate now stays resident: it
  notifies `READY=1` plus a descriptive `STATUS=standby: …`, supervises the
  peer's MCP endpoint, and retries the full startup coordination once three
  consecutive probes confirm the peer stopped serving. The unit reports
  `active` with a flat restart counter while an outside owner serves, and
  automatically reclaims the port the moment that owner exits. No unit-file
  change is required; both the shipped `Restart=on-failure` template and
  older `Restart=always` units converge. Follow-up in the same release: a
  managed flock loser that finds an alive-but-not-yet-serving
  restart-coordination holder (`ContendedAliveHolder` — e.g. two managed
  standbys re-coordinating after their incumbent dies, while the winner is
  still in reconstruct/salvage) now enters the same resident standby loop
  instead of erroring out; the erroring path burned one `StartLimitBurst=5`
  slot per `RestartSec=30` and permanently failed the loser unit in 150s
  whenever the winner's boot exceeded ~2.5 minutes. Unmanaged (human-run)
  invocations keep the fail-closed refusal.
- **`am doctor triage` can no longer report `ok / 0 findings` on a database
  stock SQLite calls malformed (#247).** Three compounding defects fixed in
  the shared integrity surfaces (used by the doctor probes, the background
  integrity guard's canonical acceptance path, and recovery promotion):
  (1) drivers that return the whole `integrity_check` output as one
  newline-joined value are now split into one detail per reported line, so a
  single "row" can no longer hide arbitrary page loss (or smuggle real
  corruption lines past a benign substring match); (2) the benign
  "`Page N: never used`" freelist-slack class is now bounded
  (`BENIGN_UNUSED_PAGE_ROW_LIMIT` = 16 rows) — mass page loss (59–72% of all
  pages in the field reports) is treated as corruption, and the REINDEX-only
  fast path is likewise disqualified; (3) every full check now passes an
  explicit `integrity_check(1000000)` limit instead of the bare pragma's
  silent 100-error cap, with bare-form fallbacks for engines that reject the
  argument. Corruption verdict strings are summarized (distinctive rows
  preserved, repetitive unused-page rows elided with explicit counts) so
  uncapped output cannot flood journals.

- **`reservation_parity` no longer fails health forever on released rows with
  no archive artifact (#244).** A *released* DB reservation missing its
  current-generation archive artifact is now informational
  (`released_missing_archive`), not drift: it is not a lock hazard (br-74sxo —
  "a missing artifact needs no heal"), the retention prune deletes the row in
  due course, and after reconstruct-from-archive the artifact typically still
  exists under the prior generation's stamp. Previously these counted as
  `missing_archive` hard drift with no fixer, so `am doctor health` returned
  rc=1 permanently on a healthy mailbox (the mirror of #173/br-5xbua). An
  *active* row missing its artifact remains drift and still self-heals via
  reconcile-on-read. Health-line examples now list drift examples before
  informational ones, so `fields=[missing_archive=…]` is no longer illustrated
  by unrelated `foreign_generation_artifact` entries.
- **Pool acquire timeout lowered below the MCP request deadline (10s vs 30s,
  #245).** `DEFAULT_POOL_TIMEOUT_MS` equaled the 30s dispatch deadline, so a
  stalled connection acquire and the outer request deadline expired in the
  same instant and every DB stall was reported as
  `stage=blocking_dispatch_unattributed` — the pool's specific "acquire
  timeout" error could never surface. The timeout bounds only the wait for a
  connection (create/init, including migrations and archive reconstruction,
  is not cut off), and a regression test now pins the ≥2x margin against
  `ECOSYSTEM_CLIENT_DEADLINE_MS`. A compile-time assert keeps the two
  constants from ever re-aliasing.
- **Timeout diagnostics report *current* health, name the queue honestly, and
  expose pure git latency (#245).** The p99 evidence attached to a dispatch
  timeout now comes from a trailing 10-minute two-epoch window
  (`p99_window_secs=600`) instead of process-lifetime histograms — one
  historical 25s stall can no longer pin the displayed p99 for a 2d8h
  process. `archive_commit_p99` is renamed `archive_commit_queue_p99` (it is
  enqueue-to-durable queue latency, not git work), `git_commit_p99` is
  exposed beside it, and both archive stages are annotated off-request-path
  since ack-fast.
- **One-time DB initialization can no longer park every tool call forever
  (#245 stall hunt).** The pool's per-database init gate previously made
  every non-initializing caller await it with no deadline: a wedged or slow
  initialization (archive reconcile behind the storage publication fence, a
  slow migration, a filesystem stall) parked every DB-needing dispatch
  thread in `futex_do_wait` while the process looked idle. Waiters are now
  bounded by the pool acquire budget (10s) and fail with an attributed,
  retryable "initialization in progress" error; only the initializer runs
  unboundedly, and an unwinding initializer resets the gate for retry.
- **`register_agent` without a name can no longer overwrite an existing
  agent (#213).** An auto-generated name that collided with an
  already-registered agent used to fall into the explicit-name upsert and
  silently replace that agent's `program`/`task_description` while acking a
  "fresh" registration. Auto-named registration now claims its name with a
  strict transactional insert-if-absent, redrawing on collision (bounded at
  16 draws) and returning a retryable `CONFLICT` on exhaustion. Explicit
  re-registration keeps its documented upsert semantics.
- **Windows extended-length (`\\?\`) storage roots: archive write-back works
  (#216).** Relative-path computation mixed plain and verbatim spellings
  (bases were normalized, canonicalized targets were not), so every archive
  artifact write failed on `\\?\` roots — 507 consecutive failures on a
  fresh DB armed the durability latch and refused the first `send_message`.
  A spelling-tolerant comparison layer (`relative_to_normalized` /
  `path_starts_with_normalized`) now backs `rel_path_cached`, the
  missing-target fallback, and attachment containment checks; plain-path
  behavior is byte-for-byte unchanged.
- **`install.sh --dest` no longer rewrites the live `agent-mail.service`
  (#243).** Service management (unit rewrite, enable, restart, legacy
  Python-unit stop, macOS LaunchAgent repair) is skipped entirely when
  `--dest` points outside the default install locations, and a new
  `--no-service` flag forces the same skip anywhere — a scratch/CI
  verification install can no longer point a production unit's ExecStart at
  a deletable directory. The service-management step now announces exactly
  which unit it is about to install/enable/restart (and which plist it is
  about to rewrite) before touching it, and
  `tests/e2e/test_install_no_service.sh` pins the gate with a fake-systemctl
  capture harness (zero systemctl invocations under a scratch `--dest` or
  `--no-service`; default-location installs still manage the service).
- **Graceful shutdown can no longer park forever (long-uptime "my TUI died"
  class).** The shutdown control message was a silent-drop `try_send` onto a
  bounded channel, and the main thread then waited on the HTTP supervisor
  with an untimed `recv()` — a wedged supervisor left the process parked
  forever with the TTY already restored. Shutdown delivery now retries
  bounded, the supervisor wait uses 15s escalation slices with a 60s budget,
  and the TUI DB-poller join is bounded (5s) with a detach fallback. The
  pure-headless park remains unbounded by design (it is the serving state).
- **A poisoned archive-backlog head op no longer wedges the backlog
  forever.** Backlog head operations are capped at 8 attempts, then
  dead-lettered to `<storage_root>/doctor/backlog_dead_letter.jsonl` (10MiB
  cap) with the durable journal quarantined under
  `.archive_backlog/failed/` (moved, never deleted) so the queue advances
  instead of retrying a permanently-failing op every 5s for days while the
  backlog fills and later writes drop.

### Added — long-uptime crash forensics

- **Every panic now leaves a structured crash marker on disk.** A
  process-wide panic hook appends message, location, thread, backtrace,
  version, and uptime to `<storage_root>/doctor/crash_markers.jsonl` (5MB
  cap) before the default stderr printer runs — a weeks-old crash in a
  closed tmux pane is no longer unattributable.
- **A TUI panic no longer kills the MCP server ungracefully.** The TUI main
  thread is wrapped in `catch_unwind`: a panic in update/render converts to
  an error routed through the normal graceful-shutdown sequence (WBQ flush,
  worker shutdown) instead of unwinding past all of it mid-frame.
- **Write-queue mutexes recover from poisoning.** A panic on any thread
  holding the deferred-write/replay-compensation mutexes previously
  cascaded panics into every other thread that touched them; they now
  recover the guard (`PoisonError::into_inner`) and degrade to
  slightly-stale bookkeeping instead.

### Changed — dependency graph

- **asupersync 0.4.4 → 0.4.5 and FrankenSQLite 0.3.2 → 0.3.4, both resolved
  from crates.io.** The `/dp/asupersync` and `/dp/frankensqlite` path
  patches are gone; asupersync 0.4.5 carries the timer-parked-task
  cancellation wakeup fix (#61). The sqlmodel family still awaits a 0.4.x
  crates.io lockstep release and is patched to a dedicated stable clone
  rather than the moving sibling checkout.

## v0.3.27 — 2026-08-14 **[Release]**

Fast-follow to v0.3.26 (next day): the silent read-only attach that made
upgraded `am` installs look broken is now an explicit choice, the whole
dependency graph moves to the latest asupersync (0.4.4) and FrankenSQLite
(v0.3.1 lineage), and the archive-read path drops its biggest per-read cost.

### Fixed — the "my TUI is gone" report

- **Plain `am` under a live server asks instead of silently degrading
  (br-mljnz).** On a terminal: `[Enter/a]` attach read-only (safe default),
  `[t]` full takeover, `[q]` quit; automation keeps the silent attach;
  `AM_ATTACH_MODE=attach|takeover|ask` presets the answer.
- **`--takeover` (and `[t]`) stops a conflicting managed service through its
  supervisor** (`systemctl --user stop` / `launchctl bootout`) so systemd
  cannot restart-fight the interactive session, and restarts it when the
  session ends. Previously takeover seized the mailbox and then fought the
  supervisor's restart loop.

### Changed — engine and runtime

- **asupersync 0.4.3 → 0.4.4** across the sibling graph (am, sqlmodel,
  beads_rust, fastmcp). Audited: no `JoinHandle::abort`/`JoinSet` usage
  anywhere, so 0.4.4's abort-vs-acknowledged-cancel change has zero exposure.
- **FrankenSQLite engine advanced to the v0.3.1 lineage** (branch
  `release-engine-0327`, commit `705ea842b`): the append-gate freelist fix
  (single-writer DDL batches no longer refused with phantom snapshot
  conflicts — the bug that blocked adopting the crates.io 0.3.1 tarball),
  the waiter-livelock fix, CONCURRENT EOF abandonment-pool fixes, and JSONB
  encoding corrections. Registry fsqlite adoption still waits for a 0.3.2
  that contains the append-gate fix.
- **Canonical evidence reads no longer consume the source family's sidecars.**
  With no checkpoint-on-drop, a hot `-wal` is the engine's normal resting
  state; receipt snapshots and the full-integrity gate now run against a
  settled private staging copy, so restores keep their quarantine evidence
  and the receipt witnesses untouched bytes.

### Fixed — v0.3.26 issue-sweep remainders

- Archive reads no longer walk the entire archive working tree (#235):
  persisted per-repo mutation epoch replaces two full `git status` walks per
  post-write read, with automatic fallback for archives written by older
  binaries.
- The periodic integrity guard no longer fail-opens on canonical
  disagreement (#214): `reconcile_with_canonical` accepts the canonical
  verdict only for the known COLLATE NOCASE class.
- The integrity guard cross-counts index vs table rows on the full-check
  cadence (#214), so index/table desync is caught at runtime instead of
  only by the CI tripwire.
- `health_check` surfaces retention/reclaim state (#210) from the same
  single inventory behind `am doctor health`; `*.stale*` artifacts gained a
  retention matcher.
- The fail-closed send profile has no token-free paths left (#237):
  `macro_contact_handshake` forwards `sender_token`; `reply_message` returns
  the same redacted receipt as `send_message` under the profile.
- `am doctor triage` distinguishes "no report yet" from "clean" (#214).

### Known

- `durability_probe_rejects_pooled_only_retained_autocommit_agent` stays
  `#[ignore]`d pending frankensqlite bd-wd904 (retained-autocommit rows
  visible to fresh connections). Production pool connections run
  `autocommit_retain=OFF`; the guarded shape cannot arise outside the
  harness.

---

---

## v0.3.26 — 2026-08-13 **[Release]**

Recovery-convergence and field-report release. v0.3.24 (the last published
build) is ~290 commits stale and is the version implicated in most open field
reports; this release ships every fix landed since, plus a sweep that closed
or advanced all sixteen open GitHub issues. Upgrading is strongly recommended:
one production mailbox crash-looped through **75,347 systemd restarts** on a
failure mode this release removes.

### Fixed — recovery must always converge

- **Reconstruct can no longer destroy real messages on canonical-id
  collisions ([#213](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/213) family, br-r6awv).**
  Cross-project (and same-project generation-reuse) collision losers were
  inserted mid-walk under `max(rowid)+1`, which could occupy a *later* archive
  file's canonical id; that real file was then silently dropped as a
  "duplicate". Collision losers now insert in a second pass after every
  canonical id is settled; duplicate detection compares identity
  (`created_ts` + subject), never numeric id alone; the canonical insert is a
  plain `INSERT` (loud, not destructive); salvage recognizes content the
  candidate already carries under a different id and maps instead of
  re-inserting.
- **A duplicate-polluted or id-reused source can no longer wedge promotion
  forever.** The promotion guard treats reduced multiplicity of a surviving
  identity as deduplication, not loss (only fully absent identities refuse
  promotion), and canonical files without parseable timestamps get a
  deterministic `created_ts` from the filename stamp instead of a fresh
  `now_micros()` per parse — which had bred new junk rows on every rebuild.
- **Every integrity surface stops under-reporting
  ([#214](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/214)).**
  The canonical-disagreement fail-open is narrowed to the known COLLATE NOCASE
  false-positive class; anything else keeps the primary probe's verdict. The
  GH#213 CI tripwire now actually probes (NOT INDEXED table scans, forced
  index plans, `sqlite_master`-enumerated indexes) instead of reading one
  btree five ways. `health_check` retains failed full-integrity evidence.
- **Archive maintenance self-heals stale locks
  ([#233](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/233),
  [#234](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/234)).**
  A `maintenance.lock` left by a killed pre-0.3.25 run is quarantined (rename,
  never delete) once provably unowned and stale; the reap is TOCTOU-hardened
  with dev/ino fingerprints; evidence records move out of `.git/objects/` and
  are pruned move-only; an evidence-write failure under disk pressure no
  longer aborts the one job that reclaims space; a fresh repo's first cycle
  no longer fails on zero packfiles.

### Fixed — security

- **Loopback RBAC no longer fail-opens behind reverse proxies
  ([#231](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/231)).**
  The unauthenticated loopback writer grant (which fixed the fresh-install
  403s) now additionally requires no forwarded headers and a loopback peer
  address, so a proxied deployment cannot hand writer roles to remote
  callers. RBAC denials are diagnosable: the 403 names the applied role, the
  denied tool, and the remediation, and emits an `http_rbac_denied` warning.
- **`reply_message` honors the fail-closed send profile
  ([#237](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/237)).**
  Reply was a token-free path to speak as any registered agent while the
  profile was on; it now routes through the same sender verification as
  `send_message`, before any write.
- **The pre-commit guard fails closed on archive-resolution errors
  ([#228](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/228)).**
  Permission/IO errors during archive resolution were coerced into
  "no project matches → allow"; a chmod-000 storage root could bypass an
  active exclusive reservation. Resolution errors now route through
  `fail_closed` (honoring `AGENT_MAIL_GUARD_MODE=warn` and advertising
  `AGENT_MAIL_BYPASS=1`).

### Fixed — field reports

- **macOS: `am mail search` and share/export work with `/var`-rooted TMPDIRs
  ([#230](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/230)).**
  One strict firmlink predicate in core now backs every symlink guard,
  including all six share-crate export/deploy paths.
- **Windows: extended-length (`\\?\`) storage roots
  ([#216](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/216)).**
  Project identity (`human_key` and slug) no longer persists the verbatim
  spelling, with upgrade-in-place matching for rows written by older builds.
- **`am inbox` no longer consumes messages it never displays
  ([#229](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/229)):**
  robot inbox listings pass `mark_read: false`; the unread-only default is
  documented.
- **`resource://file_reservations` reads through a query-only pool
  ([#241](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/241))**
  and no longer fails with "attempt to write a readonly database".
- **Idle mailboxes stop logging `no such table: atc_experiences` every
  ~6 minutes ([#232](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/232)):**
  ATC ticks gate on sidecar schema presence and never create a 0-byte
  sidecar as a side effect.
- **Inbox-event cursors bootstrap correctly
  ([#238](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/238)):**
  a monitor positioned on an empty inbox no longer gets `CURSOR_EXPIRED`
  when unrelated recipients advance the global sequence; expiry requires
  actual pruning evidence.
- **`am legacy import` preserves the source database
  ([#236](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/236)):**
  copy-only import, `immutable=1` source opens (WAL-safe via staging copy),
  failure receipts with staged partial targets so retries are unblocked, and
  an already-satisfied v20 migration is detected without triggering the
  engine schema reload.
- **Backup rotation stages instead of deleting
  ([#210](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/210)):**
  evictions move to `doctor/reclaimable/rotation-<ts>/`;
  `AM_BACKUP_ROTATION_DELETE=1` restores the old behavior; recovery
  retention is byte-budgeted per category.

### Added

- **`get_message_delivery_receipt` (tool count 39 → 40,
  [#218](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/218)):**
  message-ID-bound delivery facts — persisted, signaled (via the durable
  signal-receipt ledger), and acknowledged per recipient — now registered and
  reachable.
- **`am agents resolve-pane`
  ([#240](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/240)):**
  read-only pane-identity resolution with identity source categories.
- **`idempotency_key` on `send_message` / `reply_message` /
  `acknowledge_message`:** exact retries after timeouts replay the original
  grant instead of duplicating.
- **`am inbox-events` / `fetch_inbox_events`:** durable, restart-safe
  per-recipient delivery cursors ([#238](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/238)).

### Engine

Release binaries are built against a frozen FrankenSQLite engine (v0.3.0
lineage, branch `release-engine-0326`, commit `dabbccea6`) carrying two
verified fixes over the 0.3.0 tag: the verbatim-CREATE comment capture that
made every am-initialized database unreadable by stock `sqlite3`
("malformed database schema", bd-lgolw), and the detached drop-time WAL
checkpoint that rewrote the database family ~50ms after an async
connection's `drop()` returned, underneath forensic/salvage readers
(bd-daqmp). Every platform binary was probe-verified (`am migrate` + stock
`sqlite3` schema read) before packaging; the query-only and salvage
byte-neutrality keepers are 2/2 green at this engine.

---

## v0.3.25 — 2026-07-25 **[Tag only]**

Tagged but never published. Contains the GH#208 recovery-guard
identity/volatile split, the GH#229 robot-inbox `mark_read` fix, and the
GH#203/#204 follow-ups. All of it ships in v0.3.26.

---

## v0.3.24 — 2026-07-25 **[Release]**

Point release after v0.3.23; see the git history between the tags for the
full commit-level detail.

---

## [v0.3.23](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.23) — 2026-07-24 **[Release]**

Reliability release. v0.3.22 aborted the daemon on Linux at production scale
and made every DB read time out on macOS; both are fixed here. Upgrading is
strongly recommended for anyone on v0.3.22.

### Fixed

- **Worker threads no longer run on Rust's 2 MiB default stack ([#202](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/202)).**
  A stack overflow is not recoverable in Rust — the runtime aborts the process,
  it cannot be caught, and every other thread's in-flight work dies with it.
  `am-archive-read` ran the full archive reconstruction/salvage path on 2 MiB and
  aborted the daemon on a production-scale mailbox (~2.6k agents / ~18.3k
  messages), while the same code was fine driven from the main thread's 8 MiB
  rlimit stack.

  `am-archive-read` was not the only exposure: the operator dashboard's refresh
  worker reaches the same path via `ObservabilitySyncDb::archive_snapshot` on a
  1200 ms loop, so fixing only the reported thread would have left the daemon
  abortable. Sizing is now uniform rather than rationed to threads that look
  deep — a new `mcp_agent_mail_core::worker_stack` module holds one policy and
  all 30 `thread::Builder` chains carry an explicit stack size. Thread stacks are
  reserved address space committed lazily per page, so an untouched 32 MiB stack
  costs ~0 resident memory.

  Tunable via `MCP_AGENT_MAIL_WORKER_STACK_MB` (clamped 8..512; the legacy
  `MCP_AGENT_MAIL_READ_SNAPSHOT_STACK_MB` is still honored). `RUST_MIN_STACK` is
  folded in explicitly, since `Builder::stack_size` would otherwise override it
  and silently lower operators who had already raised it as a workaround.

- **Archive-read snapshots no longer self-invalidate on macOS ([#203](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/203)).**
  `run_build` required both the exact (content) and cheap (metadata) generations
  to agree across its snapshot-decision probes — but the probes themselves open
  the live database, and a FrankenSQLite open bumps the file's mtime on
  macOS/APFS without changing a byte. The gate could never converge: every build
  returned `Busy`, `acquire_if_needed` re-claimed in a tight loop, and every
  DB-read tool call burned the full 30 s dispatch budget.

  Publication now gates on exact-generation stability alone. This does not weaken
  the [#198](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/198)
  contract: writes are bracketed by `WriteGuard`, which advances both
  `WRITER_EPOCH` and the slot invalidation epoch on entry *and* exit, and
  publication is fenced on those epochs — so a write that landed and reverted
  inside the probe window is still caught.

- **`reply_message` no longer reports live messages as missing ([#204](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/204)).**
  It was the only message surface gating on raw project-row equality, so a
  mailbox carrying forked identity rows resolved a message everywhere except
  there. Project identity is now compared via `resolve_project_path` —
  filesystem canonicalization, which collapses exactly the
  [#194](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/194)
  aliases and nothing else. Notably *not* slug comparison: `slugify` collapses
  every non-alphanumeric run to one dash, so `/srv/app-one` and `/srv/app_one`
  share a slug, and using it would have admitted replies across distinct
  projects.

  The rejection now states that the id exists but is out of scope, without
  naming the owning project — message ids are globally sequential, so echoing
  the owner's key would let any agent enumerate ids to map projects it cannot
  access.

### Changed

- ftui 0.5 `TerminalCapabilities`: true-colour detection moved onto the
  `ColorDepth` enum.
- Dependency refresh across ~184 packages; asupersync 0.3.9.
- `docs/OPERATOR_RUNBOOK.md` gains a **Worker Thread Stacks** section covering
  the sizing policy, the tunables, and the `RUST_MIN_STACK` interaction.

### Known issues

- The 120 s archive-read `BUILD_TIMEOUT` still exceeds the 30 s dispatch
  timeout, so a build legitimately taking 31–120 s will still time the caller
  out and invite a retry (noted in
  [#203](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/203)).
- FrankenSQLite remains on the published 0.1.18 rather than the newer sibling
  checkout: `main` there is mid-async-refactor and does not compile.

---

## [v0.3.22](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.22) — 2026-07-22 **[Release]**

### Recovery correctness and portable archives

- Archive reconstruction now preserves project identities, agent recovery
  fields, registration credentials, recipients, and migration continuity
  across repeated recovery generations. Recovery candidates are validated with
  canonical SQLite before promotion, and an unreadable prior generation plus
  its sidecars is preserved under a receipted quarantine instead of being
  mistaken for a disposable staging file (#186, #187, #191).
- Export snapshot destinations are written and their FTS5 indexes finalized
  with canonical SQLite on the disposable side of the pipeline. This prevents
  FrankenSQLite's persistent pathname namespace from following a renamed
  staging database, produces stock-SQLite-clean FTS5 artifacts, preserves
  `reaper_exempt` and `registration_token` for full-fidelity archives, and
  removes registration credentials from sharing presets.
- Clean committed archive inventories are cached by Git generation, eliminating
  repeated full-tree scans on archive-aware tool and resource reads while dirty
  generations continue to bypass the cache (#192).

### Runtime, identity, and operator safety

- Non-MCP HTTP paths now run through a bounded blocking-dispatch pool, so a
  `/mail` or 404 request cannot wedge the shared MCP listener. Startup WAL/SHM
  forensic snapshots also have bounded retention (#184, #185).
- Doctor adopts the live daemon's mailbox identity, diagnoses archive/DB parity
  against the database actually being served, and detects live macOS file and
  listener owners before authorizing mutation (#193, #195).
- Filesystem case aliases collapse to one project identity on case-insensitive
  filesystems, preventing split agents, orphaned reservations, and permanent
  archive parity drift (#194).
- ATC refreshes stream bounded population summaries instead of retaining an
  ever-growing anonymous heap in a long-running daemon (#190).
- A bounded, read-only `check_file_reservation_conflicts` tool now provides an
  authoritative pre-edit conflict check with exact/glob/ancestor semantics,
  fail-closed malformed-pattern handling, and no registration or cleanup side
  effects. The implementation was independently mined from PR #196.

### Installer and updater reliability

- The installer supports stock macOS Bash 3.2 under `set -u`, persists a
  complete installer copy for later managed uninstall after `curl | bash`, and
  leaves any existing saved copy untouched if that persistence fetch fails
  (#189).
- `am update` recognizes and safely unwraps the nested release archives emitted
  by older manual release tooling while retaining checksum and path-safety
  validation (#188).

### Migration robustness and build

- A statistics-only `ANALYZE` migration whose target table is absent no longer
  aborts `migrate_to_latest`. Previously, a database whose `atc_experiences`
  table was missing (e.g. after a corrupt-page loss while the create/alter
  migrations were already recorded applied) made `v16_analyze_atc_experiences`
  hard-fail with "no such table", which wedged the whole server into DB-degraded
  mode where every MCP write returned a generic "database error". The migration
  runner now treats a missing-table `ANALYZE` as vacuously satisfied — it records
  the migration and continues — since query-planner statistics change no schema
  and no data (GH#185).
- Build fix: `ConsoleCaps::from_capabilities` was updated for the current
  `frankentui` API, whose `TerminalCapabilities` replaced the boolean
  `true_color` field with a `color_depth: ColorDepth` enum.

---

## [v0.3.21](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.21) — 2026-07-10 **[Release]**

### Fixed: the TUI no longer degrades into a mostly blank screen

- The app-level Bayesian diff advisory no longer translates `Deferred` into an
  `EssentialOnly` frame. That path did not defer terminal output; it erased most
  of the model before FrankenTUI's real diff writer saw it.
- The advisory frame budget now matches the TUI's 100 ms fast cadence instead of
  retaining a stale 16.6 ms/60 fps threshold that classified healthy frames as
  late.
- The runtime load governor is pinned to full-fidelity rendering and the
  conformal degradation gate is disabled for this operator console, so load
  shedding cannot remove visible content.
- The full traversal E2E now reconstructs the terminal with `pyte` and measures
  visible cells. Pressure, resize, flash, and a 180-second/360-step soak therefore
  fail on an actually blank screen rather than relying on emitted ANSI byte
  counts. The release candidate recorded zero empty frames and zero low-visibility
  soak samples.
- FrankenTUI's native-only runtime now routes recoverable panic boundaries
  through its backend-neutral cleanup API, fixing production builds that do not
  enable the legacy crossterm backend. An isolated native-only CI feature gate
  prevents workspace feature unification from masking this again.

### Runtime, guard, and security hardening

- Added configurable predictive TUI tick scheduling while retaining the full
  rendering invariant above.
- Migrated the workspace to Asupersync 0.3.7.
- The pre-commit guard no longer depends on Python's private
  `fnmatch.translate` output shape and fails closed on invalid glob compilation,
  including Python 3.14 environments.
- Registration-proof nonces are stored durably in the database, so replay
  protection survives process restarts.

---

## [v0.3.14](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.14) — 2026-06-20 **[Release]**

### Fixed: real on-disk `storage.sqlite3` corruption under multi-agent swarm load (#152, #156)

The published v0.3.13 binary vendored a FrankenSQLite that predated two real
engine fixes, so a busy `serve-http` host coordinating a multi-pane swarm could
corrupt `storage.sqlite3` on disk (`2nd reference to page N` / `database disk
image is malformed`), tripping archive reconstruction and the durability latch
(agents experienced this as "agent mail crashed"). This release rebuilds against
the fixed FrankenSQLite:

- **frankensqlite #115** (`d1caefb5`) — concurrent-mode double-allocate of the
  same page → `2nd reference to page`.
- **frankensqlite #118** (`f28088b6`) — in-transaction `integrity_check` false
  positive that drove the downstream reconstruct loop.
- **FK/trigger INSERT placeholder canonicalization** (`ce846249`) — the
  `expected canonicalized numbered placeholder` error on reused pooled
  connections (surfaced as `request_contact` failures).

The git-backed archive runs ahead of the SQLite index throughout, so existing
data was always recoverable via `am doctor repair`; this release stops the
corruption at its source.

### `list_agents` bounded + reservation retention; reconcile defers under lock contention (#154, #151)

- **`list_agents` is now bounded** (default/clamped limit + optional
  `active_within_days` floor) and the slow active-reservation query was rewritten
  to a non-correlated anti-join — reservation queries no longer hit the 30s
  dispatch timeout (#154).
- **`file_reservations` retention sweep** hard-prunes released/expired rows past a
  configurable horizon (`FILE_RESERVATIONS_RETENTION_DAYS`, default 30); the git
  archive retains the full audit trail (#154).
- **Integrity reconcile defers under lock/busy contention** instead of escalating
  to a spurious archive reconstruction — stopping the reconstruct storm on a busy
  multi-writer mailbox (#151).

### Fixed: 32-byte WAL false positive (doctor FAIL + startup re-quarantine + reconstruct cascade)

Two real multi-agent-host incidents (ts1 + css, 2026-06-17) traced to the same
root cause: a healthy live `serve-http` leaves an **idle 32-byte WAL** (exactly
the SQLite WAL header, zero frames) between writes, and the size-only check
treated any `1..=32` byte WAL as "header-only/truncated".

- `am doctor health`/`check` **FALSE-FAILED** ("live mailbox needs repair: SQLite
  WAL sidecar is header-only/truncated (32 bytes)") on a database the live server
  opens and serves fine.
- The startup self-heal **re-quarantined the valid WAL on every restart**, and on
  one host the quarantine + a failed probe **cascaded into a full
  reconstruct-from-archive**; repeated recovery events left ~19 GB of `doctor/`
  diagnostic dumps.

**Root cause:** a complete 32-byte WAL header with a *valid magic* is a frameless
idle WAL the current engine opens **and checkpoints** without error (now pinned by
`engine_opens_and_checkpoints_a_32_byte_header_only_wal`). The historical
GH#99/#119 workaround that quarantined 32-byte WALs was guarding a *garbage*
(all-zeros, **invalid-magic**) 32-byte WAL — the size check conflated the two.

**Fix:** magic-aware classification. New
`wal_classify::wal_sidecar_is_truncation_artifact(path)` treats `0`-byte and
valid-magic-32-byte WALs as benign, and only `1..=31`-byte or invalid-magic
32-byte WALs as removable artifacts. All six WAL quarantine/refusal sites (the
pool startup self-heal, the five CLI startup/doctor sites including the
"needs repair" health gate, and the `wal_shm_sidecar_drift` detector) plus
`classify_wal_sidecar` now route through it. GH#99 is preserved — an invalid-magic
32-byte WAL is still quarantined.

### `am tui-dump` — non-interactive freeze escape hatch (br-bvq1x.9.6, I6)

- **New `am tui-dump` command (also `am robot tui-dump`).** When the interactive
  TUI looks frozen, agents previously had no safe read-out — and killing the
  process is forbidden. `am tui-dump --format json` returns the *same*
  situational snapshot the TUI renders: it fetches the live `/mail/ws-state`
  payload (with `system_health=1`, so the per-loop heartbeat liveness verdict is
  included and names the stalled loop) and falls back to a local SQLite
  situational read when the whole process is wedged or unreachable. It is
  read-only, classified so it **bypasses the mailbox-ownership refusal** (it must
  work precisely when a live server owns the mailbox), and **always exits 0** so
  an agent can always read state instead of resorting to a kill.
- **Heartbeat surfaces now point at the read-out first.** The I1/I2 TUI
  loop-liveness report (`am robot health`) carries a new `readout_command`
  field and, on a suspected freeze, directs agents to run `am tui-dump` *before*
  the headless restart (`mcp-agent-mail serve --no-tui`).

---

## [v0.3.13](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.13) — 2026-06-15 **[Release]**

Reliability batch focused on a real multi-agent-host incident: the ATC experience
ledger growing unbounded and wedging startup, plus TUI/service coexistence. All
changes are in the trusted-local, single-user model.

### ATC experience ledger can no longer bloat the DB or wedge startup

- **Hard-cap rotation for `atc_experiences` (br-78c6m).** A host accumulated a
  2.41 GB `atc_experiences` table (629K open/unresolved rows in 6 days) that
  pegged startup (full-ledger replay) so the server never bound its port. Root
  cause: the row-ceiling sweep only evicted *terminal* (resolved/censored/
  expired) rows, so an open/unresolved backlog was never bounded. The ceiling is
  now a true hard cap — it evicts terminal rows first (rollups preserved), then
  **force-rotates the oldest rows regardless of state** when an open backlog
  still exceeds the cap. The default ceiling drops 250 000 → **50 000** and the
  sweep cadence 1 h → **15 min**.

### TUI ↔ managed-service coexistence

- **Bare `am` coexists with the systemd/launchd service (br-2y10g).** Previously,
  launching the interactive TUI while a managed service was serving the same
  storage root dead-ended ("connect to it") because the restart-coordination lock
  made the take-over path unreachable. Bare `am` now stops the *managed* service
  for the interactive session and **restarts it on exit** (every exit path),
  giving true coexistence — the service is the always-on headless backend and the
  TUI is the occasional cockpit. (Stopping/restarting a managed service is
  reversible, unlike killing a foreground peer.)

### Startup, recovery & integrity reliability (`br-bvq1x`, `br-5mnkl`)

- **Bind the HTTP listener before unbounded DB recovery (`br-5mnkl`).** A
  degraded/oversized DB no longer blocks the listener bind; `/healthz` stays live
  within the bind deadline while the DB recovers in the background and `/health`
  reports `warming_up`/`unavailable` honestly.
- **Single-owner restart-coordination lock for `am serve-http` (D5).** Racing
  (re)starts for one storage root no longer kill each other's freshly-bound
  servers.
- **Commit coalescer self-heals a broken archive HEAD** instead of wedging
  forever; **doctor** detects a HEAD pointing at a missing/corrupt object.
- **Hard row ceiling groundwork for `atc_experiences` (br-bvq1x.11.6)** and
  **last-known-healthy verified snapshot + snapshot-preferred recovery (K2)**.
- **Canonical full-check fallback** stops the integrity guard false-flagging
  `COLLATE NOCASE` indexes.
- **Reconstruct preserves canonical message IDs** (verified + locked with a
  golden test, G5).

---

## [v0.3.12](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.12) — 2026-06-14 **[Release]**

Reliability-program (`br-bvq1x`) batch plus security-review hardening from [#149](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/149). All changes are in the trusted-local, single-user model; no behavior changes for the default loopback deployment.

### Security review hardening ([#149](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/149))

A thorough static security review (clean `cargo audit`; `#![forbid(unsafe_code)]` and fully-parameterized SQL confirmed) flagged a small set of self-contained, local-relevant hardening items. The prioritized ones are addressed:

- **CSRF guard on the web UI (review #1).** Mutating `/mail/` POST routes (overseer-send, mark-read, …) now reject any request lacking `Content-Type: application/json` or carrying a cross-site `Origin`/`Referer`. The trust check uses an *exact* CORS-allowlist match (`cors_explicitly_allows`), not the permissive `cors_allows`, so the guard holds even under the dev-default empty origin list. The web UI always sends `application/json` + a same-origin `Origin`, so only forged cross-site requests are blocked; non-browser API clients (no `Origin`) pass.
- **Baseline security response headers (review #10).** Every response — including auth-bypassed health routes and 401s — now carries `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` (so a browser `?token=` query string cannot leak via the `Referer` header to CDN scripts), and `X-Frame-Options: DENY`. A strict CSP is intentionally left to a reverse proxy, since the web UI loads CDN scripts.
- **Non-loopback no-auth warning (review #2).** The server now logs a loud warning at startup when bound to a non-loopback host with no bearer token or JWT configured.
- **`SECURITY.md` + Private Vulnerability Reporting.** Added a security policy documenting the trusted-local threat model, the confidential reporting channel (GitHub PVR, now enabled), the security posture, and the hardening knobs (`HTTP_BEARER_TOKEN`, `APP_ENVIRONMENT=production`, rate limiting, reverse proxy) for deployments exposed beyond loopback.

The review's architectural items (self-asserted identity, client-chosen project keys, default-off auth/rate-limiting, permissive dev CORS) are by-design for the documented trusted-local model and are now explicitly documented as pre-exposure hardening steps rather than changed.

### Reliability program (`br-bvq1x`)

- **Corruption-specific circuit breaker (K3).** The DB layer distinguishes genuine corruption signatures from host-pressure-induced failures so the breaker no longer trips on transient overload.
- **Loss-honest salvage/recover reporting (K1).** `am doctor` reconstruct/salvage paths report what was and was not recovered instead of implying a clean rebuild.
- **Periodic SQLite maintenance (K4).** The off-hot-path integrity-guard worker now also runs passive WAL checkpoint + `ANALYZE` + `VACUUM` on independent cadences with a `journal_size_limit`, gated by `DB_MAINTENANCE_ENABLED` and per-op interval env vars.
- **Pool/FD backpressure metrics (K5).** `am robot metrics` gains a `resources` section surfacing configured pool limits, live pool/FD gauges, and repo-cache size, with actionable backpressure alerts.
- **Supervised-owner guard + `am doctor drain` (D4).** `am doctor repair`/`reconstruct` refuse (exit 3) when a live mailbox owner is present; `am doctor drain` reports `safe_to_mutate`. Startup self-heal passes `--allow-live-owner` internally, so boot behavior is unchanged.
- **Host-pressure section in `am robot health` (J1).** Surfaces disk/inode/load/memory pressure so "Database corruption detected" under load can be correctly attributed to host overload rather than mailbox corruption.
- **`health_check` decomposed into independent verdicts (C1)** plus **write-path + MCP-decode selftests (C2/C3).**

---

## [v0.3.2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.2) — 2026-05-21 **[Release]**

**Fixes the `--no-auth` write regression that broke ntm-spawned sessions** ([#131](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/131)).

`am serve-http --no-auth` is documented as "Disable bearer token authentication for this run (for local development)" — the v0.2.x contract being no auth required for *any* operation, reads and writes alike. v0.3.0/v0.3.1 regressed this: every mutating MCP tool call (`ensure_project`, `register_agent`, `send_message`, …) returned HTTP 403 Forbidden while read tools returned 200, breaking every ntm-spawned session that hardcodes `am serve-http --no-tui --no-auth` (including ntm's own startup `ensure_project`).

- **Root cause**: `--no-auth` cleared only the bearer token, which disables the bearer/JWT gate but not the RBAC layer (`http_rbac_enabled` defaults true, default role `reader` is read-only). A change between v0.2.51 and v0.3.0 incidentally flipped the `http_allow_localhost_unauthenticated` default from `true` to `false`, so localhost requests stopped being classified `is_local_ok` and RBAC began 403-ing every write tool.
- **Fix**: `--no-auth` now also enables `http_allow_localhost_unauthenticated` for that run, restoring the documented v0.2.x semantics. The global default stays `false`, so authenticated `serve-http` runs are unaffected, and `allow_local_unauthenticated` still requires an actual local peer address and rejects forwarded headers — remote callers remain unauthenticated-denied even under `--no-auth`.

---

## [v0.3.1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.1) — 2026-05-20 **[Release]**

**Windows is now a fully-built platform** (`x86_64-pc-windows-msvc`), restoring 5-platform coverage. v0.3.0 shipped 4 platforms because the TUI and the `am doctor` subsystem didn't compile for Windows; this release ports both:

- **TUI**: `mcp-agent-mail-server` now selects frankentui's crossterm-compat backend (`Program::with_config`) on non-Unix targets, since the native `ftui-tty` backend is `#[cfg(unix)]`. `ftui-tty`/`nix`/`native-backend` are gated to `[target.'cfg(unix)']`; `crossterm-compat` is used on Windows.
- **`am doctor`**: new `doctor::platform` module centralizes the cross-platform mutation/backup primitives. The Unix paths are byte-identical; the Windows equivalents preserve the doctor's hardened guarantees — reparse-point refusal (the `O_NOFOLLOW` symlink-swap defense), fd-based permission setting, deterministic UTF-16 path hashing, and NTFS symlinks. All 45 Unix-only doctor sites now route through it or are cfg-gated.
- **PID liveness on Windows** uses a conservative "assume alive unless positively dead" fallback (no `unsafe`/FFI, honoring the workspace `#![forbid(unsafe_code)]`), so the doctor never reclaims a lock from a process it cannot confirm dead.

No Unix behavior changes. Same code, now cross-platform.

---

## [v0.3.0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.0) — 2026-05-20 **[Release]**

First minor-version release. Consolidates all development since v0.2.46 (the prior CHANGELOG-versioned release) and supersedes the unpublished in-tree 0.2.47–0.2.54 version bumps. Headline changes:

- **`am doctor` world-class self-healing surface** matured through the pass-35 series: dozens of new failure-mode (FM) detectors graduated from detect-only to reversible, hash-witnessed mutations (`Op::WriteFile` / `Op::Chmod` / `Op::Rename`) routed through the single `mutate()` chokepoint — covering archive-state, db-state, mcp-config, guard-install, secrets/env, identity/contacts, and runtime-process failure modes. See the per-FM dispatcher detail below.
- **Search V3 refactor + v24 migration** dropping the recipient-cascade trigger, with search-dialect rollout and `git_binary` hardening.
- **Write-behind-queue durability hardening**: storage refuses new writes after the WBQ exhausts its retries, recoverable via `am doctor repair`; the messages id allocator now advances past the archive at startup and post-reconstruct.
- **Installer hardening** against PEP 420 namespace spoofing in Python detection.

### `am doctor` — per-FM dispatcher surface (passes 14-32)

The world-class doctor surface (added in commit `641990d8`, hardened in passes 1-13) gained a registry-backed per-FM dispatcher across passes 14-32. Every entry below is reachable from the CLI via `am doctor ...` or the library API in `crates/mcp-agent-mail-cli/src/doctor/`.

**New verbs** (handbook count: 8 → 15)

- `am doctor fixers [--format json|table]` — enumerate the FM registry (pass-14). JSON envelope schema `1.0`; table renderer for TTY.
- `am doctor fix --only <fm-id>` — invoke a single registered FM through `mutate()` with full chokepoint guarantees (pass-15). Replaces the legacy multi-detector flow for targeted recovery.
- `am doctor fix --only <fm-id> --list` — detect a single FM, no chokepoint exercised (pass-16). ~10× cheaper than `--dry-run`.
- `am doctor fix --list` (without `--only`) — detect every registered FM in one round-trip (pass-24). Emits a `{ mode: "list_all", per_fm: [...], skipped: [...] }` envelope; `skipped[]` carries FMs missing required inputs with the missing-field name.
- `am doctor explain <id>` registry fallback (pass-23) — when no recent run includes the id, falls back to `fixers::registry()` and emits the static `FixerSpec` under `mode: "registry"`. Agents can `explain` any registered FM cold without first running `--fix`.

**FM registry** (9 entries as of pass-28)

| FM id | Severity | Op | Subsystem |
|-------|----------|----|-----------|
| `fm-archive-state-files-missing-doctor-gitignore-entry` | P2 | `Op::AppendFile` | archive_state_files |
| `fm-archive-state-files-stale-archive-lock-from-dead-pid` | P1 | `Op::Rename` | archive_state_files |
| `fm-archive-state-files-stale-head-or-ref-update-lock` | P2 | `Op::Rename` | archive_state_files |
| `fm-db-state-files-world-readable-storage-db` | P0 | `Op::Chmod` | db_state_files |
| `fm-doctor-state-files-dangling-latest-symlink` | P2 | `Op::SymlinkAtomic` | doctor_state_files |
| `fm-environment_toolchain-known-bad-git-no-override` | P0 | detect-only | environment_toolchain |
| `fm-mcp-config-files-wrong-http-url-or-scheme` | P1 | `Op::WriteFile` | mcp_config_files |
| `fm-runtime-processes-stale-listener-pid-hint` | P1 | `Op::Rename` | runtime_processes |
| `fm-secrets_env_state-bak-tokens-readable` | P1 | `Op::Chmod` | secrets_env_state |

Op coverage at FM level: 6 of 7 canonical Ops (Rename×3, Chmod×2, WriteFile×1, AppendFile×1, SymlinkAtomic×1, detect-only×1). `Op::DbExec`/`Op::DbMigrate` remain stubbed in the chokepoint pending `DbConn` plumbing.

**Capabilities envelope** (`am doctor capabilities --json`)

- Pass-17 added `fm_fixers: Vec<FixerSpec>` and `fm_fixer_count: usize` so agents discovering the contract see the per-FM registry without a second call to `am doctor fixers`. The pre-existing `fixers[]` field continues to enumerate the legacy multi-detector flow.

**Drift-class closures** (three distinct duplicated-source-of-truth bugs fixed)

- Pass-18: `world_readable_token_bak::BACKUP_SUFFIX_HINTS` promoted to `pub`. Handler's candidate-discovery list now references the module's canonical const directly — broadening the detector's accept-set automatically broadens the handler's enumeration.
- Pass-19: `DispatchInputs.stale_seconds: u64` → `stale_seconds_override: Option<u64>`. Each stale-* FM now uses its own canonical `DEFAULT_STALE_SECONDS` (300/120/600s) instead of all inheriting archive-lock's 300s. Metamorphic drift test plants a 200s-old HEAD.lock and asserts ref-lock's 120s default flags it (pre-pass-19 the unified 300s would have missed).
- Pass-20: `known_bad_git_no_override` now consults `mcp_agent_mail_core::git_binary::match_known_bad` instead of a hardcoded `["2.51.0"]` list. Operators extending `AM_EXTRA_KNOWN_BAD_GIT_JSON` automatically get the new entries flagged by `--only`; `KnownBadEntry.code` (e.g. `GIT_2_51_0_INDEX_RACE`) surfaces in finding evidence.

**Chokepoint sovereignty** (pass-21, pass-22)

- Pass-21 lifted `runs::ensure_gitignore_entry` into a proper FM-level fixer (`missing_gitignore_entry`) routed through `Op::AppendFile`, so the operation is verbatim-backed-up, hash-witnessed in `actions.jsonl`, and reversible via `am doctor undo`.
- Pass-22 removed the side-effect call to `runs::ensure_gitignore_entry` from `handle_fix_only`. Unrelated `--only` invocations no longer silently mutate `.gitignore` (the regression test `dispatch_only_unrelated_fm_does_not_touch_gitignore` pins this).

**Test infrastructure**

- Pass-26: `dispatch_only_handles_every_registered_id` + `detect_only_handles_every_registered_id` iterate `fixers::registry()` and pin the invariant that every registered FM has matching dispatcher arms. Catches future "added to registry but not dispatched" regressions.
- Pass-27: `doctor_handbook_contract.rs` (4 tests) pins the handbook's verb count, required topics (`mutate()`, `actions.jsonl`, `.doctor/runs/`, etc.), and per-FM workflow recipe.
- Pass-29: `doctor_cli_smoke.rs` (5 tests) invokes the `am` binary via `std::process::Command` and verifies the JSON envelopes agents actually see (`fixers --format json`, `fix --list --json`, `explain` registry fallback, exit-code 64 for unknown ids, `fixers --format table` human readability).
- Pass-31 + pass-32: `doctor_fm_round_trip.rs` (5 tests) asserts the full `plant → fix → undo → byte-identical` lifecycle for each distinct auto-fixable Op pattern (Rename, Chmod, AppendFile, WriteFile, SymlinkAtomic) at the FM-dispatch boundary.

Test totals across the doctor surface (per-package, hermetic):

| Suite | Tests |
|-------|-------|
| `doctor_fix_only_integration` | 16 |
| `doctor_capabilities_contract` | 9 |
| `doctor_cli_smoke` | 5-8 |
| `doctor_fm_round_trip` | 5 |
| `doctor_handbook_contract` | 4 |
| `doctor_explain_fallback` | 3 |
| `doctor_selftest_integration` | 1 |
| Module tests across `fixers/*` | 60+ |

**AGENTS.md refresh** (pass-30)

`AGENTS.md`'s `am doctor` section grew from an 8-verb table to a 15-verb table, plus a new per-FM registry table (all 9 entries) and a new per-FM workflow recipe walking through enumerate → list-all → list-one → dry-run → fix → undo.

### Bug fixes

- **`am doctor` reports listener CPU samples for verified Agent Mail servers
  whose process identity is not kill-safe**
  ([#103](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/103)).
  `collect_doctor_server_runtime_diagnostics` previously reused the kill-safe
  listener PID resolver for read-only CPU sampling. That resolver intentionally
  refuses listener PIDs unless they carry an explicit Agent Mail signature or a
  current PID hint. Doctor diagnostics now use a separate
  `doctor_listener_sample_pids` helper that samples any listener PID once
  `check_port_status` has confirmed the listener belongs to an Agent Mail
  server, and rejects `Free` / `OtherProcess` / `Error`. Kill semantics are
  unchanged. Six new unit tests cover the selection matrix.
- **`am doctor reconstruct` preserves cross-project canonical id collisions
  instead of silently dropping them**
  ([#104](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/104)).
  Reconstruct previously dedup'd canonical message ids globally, so two project
  archives that independently coined frontmatter `id=N` would lose the second
  message. Now distinguishes same-project duplicates (skip, unchanged) from
  cross-project canonical-id collisions (preserve under a generated DB id and
  record a warning naming both `project_id`s). New
  `cross_project_canonical_collisions` counter on `ReconstructStats`,
  `finalize_cross_project_canonical_collision_warnings` to summarize when
  collisions exceed the per-occurrence sample limit, and an integration test
  driving the full reconstruct pipeline with two project archives sharing
  `id=7`.
- **`am self-update` now prints the official installer one-liner on every
  download or replacement failure**
  ([#102](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/102)).
  Pre-`v0.2.47` binaries on macOS arm64 cannot reliably bootstrap to the fixed
  updater because their baked-in updater hits HTTP 400 / stalls on the
  checksum fetch. Those binaries cannot be patched retroactively, but every
  future self-update failure now surfaces a copy-pasteable
  `curl … install.sh | bash -s -- --version vX.Y.Z --verify` command pinned
  to the requested version, with a v-prefix-stripping helper to avoid
  `vv0.2.50` foot-guns. Two regression tests pin the prefix-stripping
  behavior.

### Performance

- **Archive perf 3 completion-debt now has a baseline/delta artifact pipeline**
  (`br-8qdh0.3`, follow-up to `br-8qdh0.6`). Added the `br-q8yaa`
  capture-and-delta scripts plus a lightweight e2e contract so future archive
  write fixes can publish `baseline_pre_fix_*`, `baseline_post_fix_*`, and
  `fix_delta.json` artifacts covering batch-1, batch-10, batch-100,
  batch-1000, single-attachment, and 30-agent-stress points.

---

## [v0.2.46](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.46) — 2026-04-20 **[Release]**

94 commits since v0.2.45 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.45...v0.2.46)

Rolls up the git 2.51.0 concurrency hardening epic (`br-8ujfs`), ATC learning-loop
closure (`br-bn0vb`), TUI UX surfaces (`br-bb0gt`), MCP protocol-compliance coverage
(`br-a2k3h`), and five rounds of review-driven fix sweeps (`review-r1` through
`review-r5`). Tail of the release adds three post-epic git child-reap / retry fixes
and a storage ref-classification bug fix.

### Git 2.51.0 Concurrency Hardening (`br-8ujfs` epic)

- Foundation + data-driven known-bad git version catalog; `am doctor check` surfaces
  a `GIT_2_51_0_INDEX_RACE` finding (exit code 3 in CI mode) when the system git is
  flagged. See `docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race`.
- `AM_GIT_BINARY` override plumbed through every in-process git shell-out path
  (guard, share export, reservation activity probes, project identity detection).
- Pre-push hook handler wraps all three `get_push_paths` git calls in the SIGSEGV
  retry wrapper with bounded backoff/jitter aligned to `core::git_cmd::jitter_ms`.
- New `scripts/git-with-amlock.sh` wrapper for external tools and editors to honor
  the same per-repo `flock` sentinel mcp-agent-mail uses in-process.
- `am doctor fix-orphan-refs` command (F1/F3): scans for refs orphaned by the
  2.51.0 index race and can prune or archive them with `--dry-run` / `--apply`.
- Selected hot-path git operations migrated from shell-out to libgit2 (C2/C3/C4/C7)
  with a parity harness (`C5` removes the legacy `read-tree` path).
- Auto-repack schedule (F4) + B5 wrappers + D1/D2 lint guards against adding new
  un-wrapped git shell-outs.
- Docs: A6 baseline script, H2 verification runbook, README "Known-bad git
  versions" table, AGENTS.md external-git coordination section.

### ATC Learning Loop (`br-bn0vb` epic)

- v17 schema surface (`br-bn0vb.28`): additive migrations for the ATC leader-lease
  table, ATC privacy classification columns on `atc_experiences`, and rollup
  snapshot metadata storage. Upgrade tests prove fresh/latest convergence, pre-v17
  row preservation with default backfill, and archive reconstruction coverage.
- v22 compacted-history baseline columns so post-retention refreshes keep their
  stratum stats intact (`br-bn0vb`).
- Live snapshot wiring: `am robot atc` (`br-bn0vb.12`), TUI ATC screens
  (`br-bn0vb.13`), and E2E learning-loop closure tests (`br-bn0vb.14`).
- Retention + replay APIs (`br-bn0vb.5`), retention soak harness (`br-bn0vb.16`),
  `am atc explain` decision debugger (`br-bn0vb.30`), and `am atc simulate`
  dry-run CLI (`br-bn0vb.31`).
- Build-slot ATC observations wired (`br-bn0vb.8`); rollout disclaimer retired
  (`br-bn0vb.17`).

### TUI UX (`br-bb0gt` epic)

- Context-aware TUI help surfaces (`br-bb0gt.2`).
- Agent health scoring surfaces in the TUI (`br-bb0gt.5`).
- Feature flag registry scaffolding (`br-bb0gt.3`).
- Cross-epic E2E integration suite (`br-bb0gt.4`).

### MCP Protocol + E2E Coverage

- MCP protocol compliance coverage added (`br-a2k3h.8`).
- E2E harness fails fast when the server binary build fails (`br-blnuh`).
- Cross-epic integration suite added (`br-bb0gt.4`).

### Review Sweeps (r1 → r5)

- **review-r1**: 3 clippy lints across core + server; histogram metric helper
  hardening; 5 surface findings.
- **review-r2**: clock skew + poison recovery in ATC event log; stale ATC resolve
  rejection; agent-scoped ATC conflict focus.
- **review-r3**: reservation outcome eviction per-agent fallback cache; ambiguous
  TUI snapshot backfill fix; hide unrelated focused ATC rows; sweep-complete
  lint/style/test polish.
- **review-r4**: null share config treated as missing; malformed share bundle
  config rejected; root commits included in guard pre-push checks; skew-protected
  core timestamps; drop-close regression test for queries; mailbox verdict
  formatting.
- **review-r5**: saturating_sub on commit-time delta; contact TTL clamp +
  warn-on-clamp in renew; ShadowMetrics latency-delta arithmetic hardened;
  robot timestamp math hardened; mcp-agent-mail-server clippy backlog cleared.

### Bug Fixes

- **Storage** — `ref_category` no longer misclassifies `refs/stashy/*` as
  `SafeToPrune` (5b3b01c3). `SAFE_PREFIXES` was missing the trailing slash on
  `"refs/stash"`, so non-standard refs like `refs/stashy/foo` or
  `refs/stash-backup` could be auto-pruned by `am doctor fix-orphan-refs --apply`.
- **CLI + guard + core** — zombie-leak / SIGSEGV retry tail (bfc2d913, 5ba093de,
  b697c1be, 057fdde0). Three separate paths in the doctor git-version prober,
  pre-push hook handler, and guard backoff were reaping children on normal exit
  but leaking on `try_wait` / stdin-write error paths. All now force-reap before
  propagating the error. Jitter formula + doc comments aligned.
- **DB** — probe paths now treat WAL-recovery errors as retryable-unhealthy
  rather than hard errors (16cbc162); benign WAL-too-small no longer flips the
  verdict to Broken (67116e6a).
- **Core + server + CLI** — pipe-deadlock drain fix, doctor-orphan-refs rotation
  ordering, startup port probe hardening, DB agent-visibility probe, git 2.51.x
  distro-variant detection (ac012b0d).
- **Server** — bounded backup rotation; narrow test-fixture path guard;
  cargo-test-harness predicate (61609559).
- **Atc-rollup** — preserve compacted baseline fields across the canonical
  snapshot payload (3f378dfb); use `AtcRollupSnapshotRow` + full compacted-baseline
  columns on restore upsert (d4ad92b3); silence rollup-refresh WARN spam
  (01a2e7c5).
- **DbConnGuard sweep** — wrap on-demand DB connections across mail-ui TUI poller
  (003df507), ATC tool-metrics/tools/resources probes (076992a3), mcp-share deploy
  quick_check + schema-validation probes (4c12a22f), observability-sync drops
  (a2493b11), and mailbox-verdict schema probe (dc6e9856).
- **Mailbox verdict** — decisive corruption beats recovery-lock precedence in
  `compute_state_from_probes` (94ddf38d); archive-backed empty schemas detected
  in fast mode via `ArchiveStatePresence` (0d3e19b4).
- **TUI messages** — autogenerated coordination messages (file reservations,
  contact requests, system notifications) hidden by default (a8fe7358).
- **Metrics** — `tantivy_last_update_us` now uses raw wall-clock, not the
  skew-protected clock (143c067a).
- **Setup** — propagate CSPRNG failures instead of silently returning empty or
  panicking tokens (57120a21).
- **Health** — `/health` body distinguishes recovering from corrupt (f49ffb65);
  `/health/durability` regression net added (36fdaed6).
- **Service** — systemd restart on re-install so the new unit takes effect
  (582b6ccd); macOS launchd `ThrottleInterval=30` to match systemd `RestartSec`
  (28bb678c).
- **Install** — capture service status output unconditionally in readiness-check
  failure path (5c07bb28); thread bearer token through `setup_claude_code_mcp_via_cli`
  (fb8d372d); clarify Claude Code vs Claude Desktop candidate-scan comment (e0607707).

### Performance

- **Archive batch write performance fixed** (`br-8qdh0.6`). Warm `batch-100`
  message writes now measure ~238ms p95 and ~242ms p99, improving the README
  historical baseline from roughly 1076ms p95 to an under-budget steady-state path.
- **Archive read path baselines characterized** (`br-8qdh0.13`).
- Artifacts: perf baselines refreshed from the 2026-04-18 rerun (e6bf19ac);
  legacy archive-baseline, flamegraph, and extended-dim perf files untracked
  (1603412b).

### Documentation

- **Documentation alignment sweep completed** (`br-o217s.7`). Final consistency
  checks removed stale count phrasing from the operator docs, updated the
  rollout playbook to the current 37-tool / 25-resource surface, clarified
  legacy incident notes as pre-16-screen historical artifacts, and kept the
  conformance audit/README aligned with the live router.
- **Reality-check epilogue completed** (`br-ldpdv`). Re-ran the post-epic audit
  against the live repo surface, confirmed the five original reality-check epics
  are closed, fixed deferred-browser doc drift so `/mail/ws-state` stays
  documented as a supported robot/TUI polling endpoint while `/web-dashboard/*`
  and `/mail/ws-input` remain deferred.

### Deferred

- **Browser TUI mirror and WASM frontend deferred** (`br-il53l`). After
  evaluating the ship-or-retire decision (br-il53l.1), the browser TUI mirror
  (`/web-dashboard/*`, `/mail/ws-input`) and the standalone
  `mcp-agent-mail-wasm` crate are deferred indefinitely. All six browser-mirror
  HTTP endpoints now return `501 Not Implemented` with a pointer to
  `docs/SPEC-browser-parity-contract-deferred.md`. The shared `/mail/ws-state`
  polling endpoint remains live for robot/TUI snapshot consumers and should not
  be treated as proof that browser parity shipped.
  - The `mcp-agent-mail-wasm` crate has been moved to `experimental/` and
    removed from workspace members.

### Style / Internals

- Cargo fmt + `const fn` / `#[must_use]` hardening sweep across core, server,
  tools (fd72998e).
- `db/atc_queries` rustfmt + naming consistency pass across the rollup hot path
  (df9b1492).
- `.gitignore` narrowed `test_*.rs` re-include and added `atc-bench` /
  `target-local` / `target-review` dirs (bfbb2cb7).

---

## [v0.2.45](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.45) — 2026-04-18 **[Release]**

Re-pin of `asupersync` to commit 310ff61f and version bump. See compare view for the
full content delta against v0.2.42.

---

## [v0.2.43](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.43) — 2026-04-17 **[Tag only]**

## [v0.2.44](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.44) — 2026-04-18 **[Tag only]**

---

## [v0.2.42](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.42) — 2026-04-16 **[Release]**

Fixes Windows-native `am.exe serve-http` startup (#93) and a related side-effect that
silently corrupted MCP client configs on every failed boot.

### Bug Fixes

- **Windows native `am.exe serve-http` was unusable** (#93). On a fresh Windows install
  with no prior `~/.mcp_agent_mail_git_mailbox_repo`, startup crashed with
  `unable to open database file: 'C:/\\'` (os error 161, `ERROR_BAD_PATHNAME`).
  Root cause: `fs::canonicalize` on Windows returns a `\\?\C:\…` UNC verbatim path;
  embedding it into `sqlite:///{path.display()}` produced a URL whose literal `?` was
  then split by the query-string parser, truncating the path to `/\\` (3 bytes).
  - The URL parser (`sqlite_path_component`) now skips `?` markers that are part of a
    `\\?\` UNC verbatim prefix.
  - URL construction goes through a new helper, `disk::sqlite_url_from_path`, that
    strips the UNC prefix and normalizes separators to `/`.
  - The parser also peels a stray leading `/` before a Windows drive letter
    (`/C:/...` → `C:/...`).
- **Failed `serve-http` startup silently rewrote MCP client configs** to point at the
  port that never opened (#93 secondary). The setup-self-heal step now runs *after*
  the startup preflight passes — a crashed boot leaves Codex/Gemini/Claude Code MCP
  configs untouched.

### Internals

- New helper: `mcp_agent_mail_core::disk::sqlite_url_from_path(&Path) -> String`. Use
  this everywhere a SQLite database URL is built from a `Path` instead of
  `format!("sqlite:///{}", path.display())`.
- Runtime callsites updated to use the helper:
  `Config::from_env` default `database_url` derivation,
  `pool.rs::capture_automatic_recovery_bundle`,
  `mcp-agent-mail-tools::lib` snapshot pool setup,
  `mcp-agent-mail-tools::resources` snapshot pool setup.

---

## [v0.2.41](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.41) — 2026-04-16 **[Release — Latest]**

Dependency refresh aligning with latest franken* sibling-repo versions.

### Dependencies

- Bump `ftui*` family from 0.3.0 to 0.3.1 (frankentui)
- Bump `frankensearch-core` from 0.1.1 to 0.1.2
- Bump `frankensearch-embed` from 0.1.2 to 0.1.3
- Bump `frankensearch-index` from 0.1.1 to 0.1.2
- Bump `toon` (tru) from 0.2.0 to 0.2.2
- Bump `beads_rust` from 0.1.38 to 0.1.42
- Bump `franken-agent-detection` from 0.1.0 to 0.1.3

---

## [v0.2.40](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.40) — 2026-04-16 **[Release]**

Minor timestamp-normalization and attachment-badge fixes. See commits c3b26a77, 03516ddc, b1e4ddd7, 0baa17f4.

---

## [v0.2.39](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.39) — 2026-04-12 **[Release]**

81+ commits since v0.2.38 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.38...v0.2.39)

Comprehensive security hardening, FrankenSQLite migration completion, orphaned-data resilience, and SQLite recovery sidecar infrastructure. This release makes Agent Mail significantly more robust against symlink escape attacks, crashed-agent data corruption, and production database recovery scenarios.

### Security Hardening

- Reject symlink escape attacks across all filesystem I/O surfaces: share bundles, deploy verification, archive paths, TUI persistence, crypto signing, PID hint files, and database paths
- Harden listener PID hint file writes against `AlreadyExists`/`PermissionDenied` race conditions with atomic retry
- Reject parent directory traversal (`..`) in TUI persist paths to prevent path escape
- Validate TUI preset names against empty/collision and reject symlinked DB paths in share operations
- Extend symlink-safe validation to age crypto, deploy history, and bundle export config paths
- Stop swallowing serde errors; fail hard on chmod errors in share operations

### FrankenSQLite Migration

- Complete FrankenSQLite migration: remove sqlmodel-sqlite/libsqlite3-sys C dependency
- Replace sqlite3 CLI usage in installer/scripts with FrankenSQLite-backed `am` tooling helpers
- Route file-backed ATC experience IO through canonical SQLite path
- Use `open_sqlite_file_with_lock_retry` instead of recovery opener for WAL checkpoint

### Orphaned-Data Resilience

- Comprehensive orphaned-agent, orphaned-project, and orphaned-sender resilience across all query and rendering paths (db, cli, server, tools, storage)
- Tolerate orphaned project metadata and recipient rows in inbound/outbound queries, mail explorer, and global inbox
- Trim agent names and drop blank entries during `recipients_json` sync
- Keep `recipients_json` visible when agent row is missing during reconstruct
- Route project resolution through `context::resolve_project` for synthetic-id tolerance

### SQLite Recovery & Sidecar Infrastructure

- SQLite recovery sidecar consolidation: stage-then-swap archive restore with rollback-journal awareness
- Mailbox health verdict with archive snapshot fallback for suspect live-db reads
- Transactional salvage merge and ATC schema repair migrations
- Embedded-database archive support with symlink-safe reset
- `am doctor` repair preservation improvements and temp artifact tracking

### Server & Web UI

- Staged static export pipeline with Ed25519 signing
- Auth-helper URL generation for inbox and unified-inbox client-side actions
- Consume mailbox verdict for primary read surface + `ack_filter` query param
- Parse repeated + comma-separated importance filter params for `/search`
- Convert `mail_claims.html` from layout-extending block to standalone partial
- Filesystem-first project resolution for archive routes
- Filtered archive directory scan with symlink-safe snapshot rebuilds

### CLI & Robot Mode

- Extended malformed-attachments sentinel to robot output + TUI attachment/message/thread views
- Safe atomic share-update pipeline with expanded robot mode
- Doctor sidecar cleanup, temp artifact tracking, and migrate open path
- Shared malformed-JSON sentinels + synthetic-project-id tolerance across tools/doctor/TUI

### Build & Infrastructure

- Updated frankentui dependency versions from 0.2.1 to 0.3.0
- Updated beads_rust dependency version from 0.1.14 to 0.1.38
- Conditional compilation fixes for tantivy benches and featureless builds
- Runtime warnings and documentation for concurrent mode snapshot drift (GH#65)
- Installer TOML config writer made idempotent with duplicate entry handling
- Install-local.sh added with jq-first JSON parsing

---

## [v0.2.13](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.13) — 2026-03-22 **[Release]**

8 commits since v0.2.12 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.12...v0.2.13)

Hardens Python-to-Rust migration and startup so installed `am` keeps using the migrated mailbox database instead of being hijacked by repo-local `.env` files. Also makes doctor/migration recovery much more tolerant of SQLite snapshot conflicts and stale legacy schema state.

### Changes

- Prefer installer-managed user config over working-directory `.env` files during startup and doctor flows
- Treat SQLite snapshot-conflict errors as recoverable so startup and doctor repair fall back into recovery instead of bailing out
- Reconcile legacy migration edge cases where `recipients_json` already exists or stale message FTS triggers still point at missing `fts_messages`
- Honor the documented ATC shrinkage cap when between-group variance collapses to zero instead of silently using uncapped full pooling
- Add a hermetic regression test that reproduces the exact hostile cwd `.env` override scenario and proves the installed database path still wins

---

## [v0.2.12](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.12) — 2026-03-21 **[Release]**

2 commits since v0.2.11 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.11...v0.2.12)

Dependency version bump for crates.io publish cascade. Packages the FrankenSQLite WAL compatibility fixes from v0.2.10 and v0.2.11 into a clean release with aligned workspace dependency versions.

### Changes

- Updated workspace dependency versions so all crates can be published to crates.io in the correct order ([b679466](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b679466468648e09e3700c752c28f953f8242064))
- Updated Cargo.lock dependency versions ([b6819d8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b6819d8))

---

## [v0.2.11](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.11) — 2026-03-21 **[Release]**

1 commit since v0.2.10 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.10...v0.2.11)

Fixes the root cause of "database is busy (snapshot conflict on pages)" errors when installing on machines with existing Python mcp_agent_mail databases.

### Fix: Python Database Migration WAL Checkpoint

The migration checkpoint function was using FrankenSQLite (`FrankenConnection`) to open Python-created databases. FrankenSQLite cannot read C SQLite's WAL format because they use different page formats. When the Python database had uncheckpointed WAL pages, the migration copied the main file without those pages, leaving B-tree references to nonexistent pages.

- `checkpoint_sqlite_for_copy()` now uses C SQLite (`SqliteConnection`) to properly flush the Python WAL before copying ([12d5ed5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/12d5ed5351596cac6a789c35a3320a21ee7558c3))
- `inspect_db_signature()` also uses C SQLite for robustness when examining Python source databases
- Installer `copy_sqlite_snapshot()` now fails hard if WAL checkpoint fails instead of silently producing a truncated copy
- Added `FramedCodec::with_frame_hooks` to asupersync gRPC codec

**Recovery**: `curl -fsSL ".../install.sh?$(date +%s)" | bash -s -- --version v0.2.11 --force`

---

## [v0.2.10](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.10) — 2026-03-21 **[Release]**

3 commits since v0.2.9 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.9...v0.2.10)

Fixes FrankenSQLite `BusySnapshot` crash-recovery bug that prevented `am` from starting after an unclean shutdown.

### Fix: FrankenSQLite BusySnapshot on Crash Recovery

During pager refresh, FrankenSQLite trusted the database header's `page_count` field without cross-checking the actual file size. A crash between growing the file and updating the header left `page_count` stale. On reopen, the MVCC snapshot boundary was set too low, rejecting the legitimately-committed page as a BusySnapshot conflict.

- Pager refresh now uses `max(header.page_count, file_size / page_size)` to include all physically-present pages ([3011762](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3011762))
- Clippy compliance, dead code removal, and test modernization across all crates
- Also fixes `am doctor repair` hanging with the same error

---

## [v0.2.9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.9) — 2026-03-21 **[Release]**

4 commits since v0.2.8 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.8...v0.2.9)

Bundles the v0.2.8 HTTP server deadlock fix with additional clippy/lint fixes and sibling dependency repairs.

### Changes

- Glob case sensitivity and ATC pattern counting logic fixes ([b1836d0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b1836d0))
- Clippy lint fixes for ATC labeling and VoI control ([118081b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/118081b))
- Clippy and lint fixes across core, guard, and search-core crates ([ae3d572](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ae3d57211ae18594784e17e654931f64ecc01a77))

---

## [v0.2.8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.8) — 2026-03-21 **[Release]**

152 commits since v0.2.7 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.7...v0.2.8)

Largest release since v0.2.0. Introduces the ATC learning stack, fixes a critical HTTP server deadlock, overhauls the web dashboard, and lands hundreds of correctness and performance fixes.

### Critical Fix: HTTP Server Hang Under Concurrent Load

Fixed a compound deadlock that caused the HTTP server to become permanently unresponsive when multiple MCP clients connected simultaneously (e.g., Codex + Claude Code). Manifested as Codex timing out after 30 seconds, curl connecting but receiving 0 bytes, and `/health/liveness` hanging.

**Root cause** -- three interacting issues:

1. `dispatch()` was synchronous, blocking async worker threads on every JSON-RPC request while doing DB operations
2. ATC operator runtime auto-selected io_uring, causing `handle_reserve_ticket` D-state hangs in the kernel
3. `push_event()` used `std::thread::sleep()` in the HTTP handler's async context, blocking workers for up to 14ms per request

**Fixes**:

- `dispatch()` offloads sync router/DB work to `spawn_blocking` ([c406943](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c406943))
- ATC operator runtime explicitly uses epoll reactor, eliminating io_uring kernel hangs
- HTTP handler uses `push_event_async()` instead of blocking `push_event()`
- HTTP runtime configured with a dedicated blocking thread pool

### ATC (Agent Traffic Control) Learning Stack

A complete causal inference and adaptive coordination engine, extending the ATC module introduced in v0.2.7 with a full learning stack built across 14+ modules:

- **Experience data model**: experience tuple data model, learning baseline, schema migration ([df0071b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/df0071b))
- **14 learning modules**: labeling, risk budgets, regime detection, adaptation policies, and more ([7271588](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7271588))
- **Experience persistence**: queries, runtime integration, system health display ([b85aeae](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b85aeae))
- **Effect semantics**: preconditions, cooldown, escalation, semantic messages, family-based messaging ([7f29595](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7f29595), [6f96266](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6f96266))
- **Policy promotion**: doubly-robust evaluation, confidence sequences ([edb871b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/edb871b))
- **VoI control**: value-of-information, identifiability debt, safe experiment design ([52dbff7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/52dbff7))
- **User surfaces**: state taxonomy, noise control, safe defaults, golden workflows ([46da9f0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/46da9f0))
- **ATC integration**: engine wired into server runtime with 6 alien-artifact tracks ([206bb26](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/206bb26))
- **TUI ATC dashboard**: agent/decision/detail panels with screen registry integration ([8d32023](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8d32023), [65ea16c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/65ea16c))
- **Operator telemetry**: unified tick+summary, enriched operator telemetry, heap-scheduled review loop ([b746eb3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b746eb3), [d1cb310](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d1cb310))
- **Numerical stability fixes**: overflow, unsafe subtraction, shrinkage bias, DR variance, e-process predictability, burst-rate false-positive floor ([cdbc31d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/cdbc31d), [2b3fde2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2b3fde2), [43e94e6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/43e94e6), [d5e5f15](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d5e5f15))

### Web Dashboard Overhaul

- Full HTML/JS client with screen metadata and delta streaming ([6654f2d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6654f2d))
- `/stream` endpoint with long-poll, delta journal, and viewer tracking ([158b323](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/158b323))
- Artifact-graph traceability, policy bundles, and effect plans for ATC ([8224148](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8224148))
- Conflict graph management, liveness feedback tracking, pattern-overlap detection ([5021045](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5021045))

### Messaging and Identity

- Exposed `list_agents` MCP tool and pinned service install paths ([b848567](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b848567))
- Identity module expansion and reconstruct overhaul ([09f114b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/09f114b))
- Schema expansions and search service query capabilities ([1ccd3fb](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1ccd3fb))
- TUI compose view expansion ([ed4a8ab](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ed4a8ab))
- Native SQLite sync inbox queries and CLI direct-check path refactor ([402b4de](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/402b4de))
- Local `send_message` fallback, reconstruct expansion, ATC routing refinements ([17be55a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/17be55a))

### Performance

- Replace O(n^2) `Vec::contains` dedup with `HashSet` in recipient handling ([943d398](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/943d398))
- `Vec` to `VecDeque` for bounded collections across DB, server, and search-core ([7c0e4d6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7c0e4d6), [5b081b9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5b081b9), [b40d9ac](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b40d9ac))
- Eliminate unnecessary string allocations in case-insensitive comparisons ([0b14d24](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0b14d24))
- Byte-level ASCII lowercasing for sort comparisons ([bcddf21](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bcddf21))
- Raise Tantivy writer arena from 3MB to 15MB minimum ([4de5d7b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/4de5d7b))
- Batch `mark_messages_read` eliminates N+1 in `fetch_inbox` ([9e5e468](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9e5e468))
- Arc-share cached rows, batch `inbox_stats` rebuild ([bed67a2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bed67a2))
- BTreeMap reservation index, dedup thread IDs, canonicalize-once attachments ([8f8a494](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8f8a494))
- Sampled write maintenance on hot reads to reduce lock contention ([f0706fa](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f0706fa))
- Indexed reservation conflict detection with BTreeMap prefix lookups ([1d9265f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1d9265f))
- Amortize base-dir canonicalize in `process_attachments` ([eacc4f9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eacc4f9))

### Security

- Untrack MCP config files containing bearer tokens ([89f5e9b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/89f5e9b))
- SVG XSS prevention in share pipeline ([d83cdfd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d83cdfd))
- 1MB file-size limit for reservation JSON in archive scanner ([1eb10dd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1eb10dd))
- 50MB safety limit on message file reads ([ae88f77](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ae88f77))
- Skip all symlinks during ZIP bundle collection to prevent directory traversal ([c7107b3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c7107b3))
- Harden bundle security and normalize GitHub repo detection ([d8b308b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d8b308b))
- XSS regression tests and pre-computed thread URLs ([28f51ab](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/28f51ab))
- Remove client-side markdown fallback to harden XSS surface ([6551984](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6551984))

### Correctness

- `saturating_sub` for all timestamp arithmetic across core, ATC, and CLI ([df98813](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/df98813), [2b890e3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2b890e3), [0f78f01](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0f78f01))
- Preserve error context in 11 `map_err(|_|)` lock-poisoning handlers ([0e68b09](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0e68b09))
- Replace `unreachable!()` with error return in coalesce joiner on leader panic ([711339a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/711339a))
- Unicode-width for correct table column alignment with CJK and emoji ([a057d74](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a057d74))
- Fix dotenv parser emitting literal backslash before escaped char ([94d9e5b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/94d9e5b))
- Fix integer overflow, f64 Infinity injection, and cleanup race condition ([ab139d5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ab139d5))
- Rebuild `inbox_stats` from ground truth, fix S3-FIFO cache leak ([57eeedd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/57eeedd))
- WASM error handling for HTTP poll init, WebSocket wait, and bootstrap ([a66895f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a66895f))
- Database connection lifecycle management improvements ([4043bea](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/4043bea))
- Missing v3 timestamp migrations for `message_recipients`, `agent_links`, and `project_sibling_suggestions` ([ec662d8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ec662d8))
- BOCPD input validation, recovery hardening, snapshot PK fix ([d83cdfd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d83cdfd))
- Age encryption pre-flight checks and robot batch-size controls ([55a9c8f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/55a9c8f))

---

## [v0.2.7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.7) — 2026-03-16 **[Release]**

53 commits since v0.2.6 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.6...v0.2.7)

Major expansion introducing the ATC (Agent Traffic Control) module, XDG Base Directory support, comprehensive security hardening, and S3-FIFO cache improvements.

### ATC (Agent Traffic Control) Module

The foundational ATC infrastructure -- a runtime coordination engine for managing agent interactions:

- **Decision core**: martingale-based anomaly detection, calibration guard, conflict graph, liveness feedback ([bf23258](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bf23258))
- **CalibrationGuard**: safe-mode policy engine for throttling aggressive agents ([0952c27](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0952c27))
- **Load router**: learning-augmented capacity model for request distribution ([22b5625](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/22b5625))
- **Predictive coordination**: intelligence layer for proactive conflict avoidance ([7221f97](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7221f97))
- **Advanced algorithms**: VCG mechanism design, queueing theory, PID controller ([b870d8f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b870d8f))
- **Robot CLI**: `am robot atc` subcommand for ATC status queries ([aeacb1a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/aeacb1a))
- **Server integration**: ATC module wired into server runtime, e-value overflow guard ([9ba101f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9ba101f), [e708241](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e708241))
- **E2E testing**: test script, load router tests, 147 total ATC tests with 29 edge case tests ([5f4404d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5f4404d), [f028279](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f028279))

### Security Hardening

- Crypto passphrase leak prevention, SQL identifier escaping, Unicode path folding ([badeec3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/badeec3))
- Harden PID hint file against symlink TOCTOU attacks ([efb4f58](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/efb4f58), [dc64384](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/dc64384))
- systemd TOCTOU fix, unit file parsing, PID hint timestamps ([965364c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/965364c))
- SQL identifier validation to prevent injection via table aliases ([9ed3ec8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9ed3ec8))

### Search and Caching

- SQL plan search for Agent/Project doc kinds, cursor pagination, query facets ([f1a202d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f1a202d))
- S3-FIFO cache sequence tracking to prevent ghost entry amnesia ([f9154d4](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f9154d4))
- Increased cache capacities and `CompiledPattern::cached()` for hot-path pattern compilation ([e90e95d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e90e95d))

### CLI and Operations

- XDG Base Directory spec support with backward compatibility ([722d91f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/722d91f))
- Composite tmux pane IDs to prevent collisions in multi-session setups ([b19147e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b19147e))
- Auto-stop conflicting systemd service before launching interactive TUI ([3313205](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3313205))
- Enriched PID hint files with executable path for robust process identity ([1f08ef8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1f08ef8))
- Robot attachments read path and hardened query patterns ([5168fa1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5168fa1))
- Generalized managed service conflict detection for systemd and launchd ([5deedc5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5deedc5))

### Database and Server

- Project boundary enforcement in `get_messages_details_by_ids` ([0b18c8a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0b18c8a))
- Cache-bypassing agent lookup, named columns for inbox, connection leak fixes ([304ae54](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/304ae54))
- Cached identity resolution, binary search for name validation ([689bce3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/689bce3))
- Deadlock detection perf, TUI safety, HTML escaping in tests ([646a9d6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/646a9d6))
- Denormalize `recipients_json` on message insert ([45052f1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/45052f1))
- WBQ fallback paths and synchronous fallback when WBQ is unavailable ([b51578f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b51578f), [1dbad33](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1dbad33))
- Service install hardening and port-kill safety ([df11d13](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/df11d13))

---

## [v0.2.6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.6) — 2026-03-14 **[Release]**

3 commits since v0.2.5 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.5...v0.2.6)

Performance-focused patch release targeting TUI responsiveness and static file security.

### TUI Performance

- Throttle full DB snapshots when `PRAGMA data_version` is unavailable, reducing unnecessary I/O ([2f2e92c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2f2e92c))
- Extend poller sleep interval when `PRAGMA data_version` unavailable ([2a3c2ca](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2a3c2cad04ace770930fdf480caf257be14c158a))

### Security

- Harden static file serving against symlink traversal; deduplicate dashboard footer widgets on dense surfaces ([f4f9a39](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f4f9a39))

---

## [v0.2.5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.5) — 2026-03-14 **[Release]**

3 commits since v0.2.4 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.4...v0.2.5)

Patch release fixing project-qualified agent identity and TUI theme correctness.

### Changes

- Project-qualified agent identity, theme cache correctness, and dispatch hardening ([b752fff](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b752fff))
- Reformat agents screen for rustfmt compliance; update tests for project-qualified identity ([9a98f4b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9a98f4b))

---

## v0.2.4 — 2026-03-13 **[Tag only]**

59 commits since v0.2.3 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.3...v0.2.4)

Major hardening release focused on symlink security, SQLite disaster recovery, installer robustness, and cross-project message isolation.

### Symlink Security Audit

Comprehensive symlink-safe filesystem traversal across the entire codebase:

- SQLite backup/recovery hardened against symlink traversal ([5e7cddc](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5e7cddc))
- Guard plugin rewritten to read archive directly, hardened against symlinks ([c99cc0d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c99cc0d))
- Symlink-safe static file serving via `O_NOFOLLOW` ([9935a20](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9935a20))
- Bundle export and deployment hardened against symlink traversal ([6072f6e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6072f6e))
- Consolidated `PRAGMA` checks and explicit `storage_root` threading ([7a7e7e0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7a7e7e0))

### SQLite Disaster Recovery

- Salvage-based disaster recovery with archive reconstruction and merge ([dcd2a47](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/dcd2a47))
- Reconstruct file reservations from archive storage ([70dc440](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/70dc440))
- Eliminate per-connection `journal_mode WAL` contention; harden write-retry logic ([fbb4baf](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/fbb4baf))
- MVCC retry extraction, BusySnapshot recognized as MVCC conflict ([5a5f715](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5a5f715), [1b1e029](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1b1e029))

### Installer Hardening

- Legacy launcher takeover shims, i64 DB adoption, env parsing hardening ([dfbefe7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/dfbefe7))
- Detect aliases in sourced files (ACFS) and kill all Python processes during upgrade ([80137e9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/80137e9))
- Repair same-version installs when `am` is still shadowed by Python ([9215e86](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9215e86))
- Harden PATH management for login shells and non-interactive zsh ([a60a46c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a60a46c))

### Cross-Project Isolation

- Cross-project message isolation, multi-addr health check, batch tracking ([ec7a7c4](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ec7a7c4))
- Server-first dispatch for `send`, `reply`, and `inbox` commands ([652c245](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/652c245))
- Sender vs agent filtering distinction for outbox queries ([60b741f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/60b741f))

### Operations and Monitoring

- Database lock probe and startup pipeline hardening ([27e46f0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/27e46f0))
- Release bundle validation, graceful TUI signal termination ([00909be](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/00909be))
- Coalescer depth counter underflow fix with saturating CAS decrement ([eb413ac](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eb413ac))
- IPv4/IPv6 wildcard normalization for client connections ([019f1b6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/019f1b6))
- TUI palette caching, contrast tuning, rendering optimizations ([7359497](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7359497))
- Archive-snapshot robot fallback, inbox resilience ([331e920](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/331e920))

---

## [v0.2.3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.3) — 2026-03-11 **[Release]**

93 commits since v0.2.2 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.2...v0.2.3)

Large feature release with DbConnGuard RAII wrapper, doctor subcommand enhancements, TOML config support, BCC messaging, and extensive query/storage improvements. Also enables Windows builds by removing the optional kafka dependency.

### Database Layer

- `DbConnGuard` RAII wrapper for explicit SQLite connection cleanup ([14867d3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/14867d3))
- All short-lived pool/search connections wrapped in `DbConnGuard` ([228891d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/228891d))
- `release_reservations_by_ids_returning_ids` and search cache authorization keying ([a0b1742](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a0b1742))
- Centralized clock-skew-aware timestamps module ([c51dc23](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c51dc23), [000c29e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/000c29e))
- Batch thread participant lookup and unified inbox pagination fix ([5bae811](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5bae811))
- Denormalize `recipients_json` on message insert ([45052f1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/45052f1))
- Correct `sqlite://` URI path parsing to preserve absolute paths ([ba01bb5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ba01bb5))
- Race condition fix in `now_micros()` monotonic clock ([4a71727](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/4a71727))

### CLI and Doctor Enhancements

- Foreign key integrity checks and orphaned recipient cleanup ([d69bbf7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d69bbf7))
- `sqlite3 quick_check` rescue and new integration tests ([4502029](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/4502029))
- SQLite health probes, doctor orphan detection, MCP config URL repair ([890e40d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/890e40d))
- Recognize `-cli` binary names and fall back to listener PIDs for alias-launched servers ([65e7e62](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/65e7e62))
- Harden service install and tighten port-kill safety ([df11d13](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/df11d13))

### Configuration and Tooling

- TOML config support, HTTP URL mode detection, pool-scoped caching, provider prefix stripping ([dd71439](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/dd71439))
- Tool-aware MCP config rewriting, SQLite lock retry, snapshot hardening ([08876b7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/08876b7))
- Codex integration switched from stale JSON/HTTP to TOML/stdio config ([ca6e0dc](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ca6e0dc))
- ATC engine configuration via 10 environment variables ([f70c0f6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f70c0f6))

### Messaging and Agent Resolution

- Agent name normalization to PascalCase across all entry points ([0d3136e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0d3136e), [84a938e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/84a938e), [be8fcce](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/be8fcce))
- LLM integration hardening: Anthropic auth, JSON extraction, char boundary safety ([758604c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/758604c))
- BCC redaction in inbox copies, proper BCC archival ([f46de2f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f46de2f))
- Strict validation for limits, repo paths, and ordered-prefix parsing ([595af1d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/595af1d))
- `send_message` alias normalization and stricter unique constraint detection ([af0b0e6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/af0b0e6))
- Numeric thread reference resolution for root messages ([3abbe85](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3abbe85))

### Server Architecture

- Async supervisor architecture, SQL query caching, MVCC async backoff ([038e53c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/038e53c))
- Robust HTTP supervisor lifecycle with timeout-escalated shutdown, watchdog thread, and retry respawn loop ([43f6a11](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/43f6a11))
- Per-recipient read tracking, importance filter propagation, live mark-read in mail UI ([f5530ba](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f5530ba))
- Reservation enrichment with project and `created_ts` fields ([0c4df4c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0c4df4c))

### Other Highlights

- Removed optional kafka feature from asupersync dependency, enabling Windows builds ([a813517](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a81351741a39b876156b45103f07ca55ec3cb5b7))
- Sender_id wired through search pipeline and result models ([cd9c5d6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/cd9c5d6), [0c75080](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/0c75080))
- TOON encoder deadlock prevention, reservation race fix ([9533b47](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9533b47))
- Fail-closed activity probes and precise stale release reporting ([af0b0e6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/af0b0e6))
- Navigation views for robot: urgent, ack, tooling, identity, config ([de53a3a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/de53a3a))

---

## v0.2.2 — 2026-03-07 **[Tag only]**

84 commits since v0.2.1 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.1...v0.2.2)

Massive stabilization release. Unifies case-insensitive agent resolution across the entire stack, adds durability probes, introduces TUI V3 screens with batch operations, and applies deep query/storage hardening.

### Case-Insensitive Agent Resolution

Unified case-insensitive agent name matching across DB, CLI, server, tools, and resources, preventing duplicate agent registrations from case mismatches:

- Comprehensive cross-crate resolution ([baa350f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/baa350f), [516a089](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/516a089), [f5ab55e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f5ab55e))
- Robot deduplication for case-insensitive name collisions ([7fee0ee](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7fee0ee))

### TUI Improvements

- Shared tick event batching, interior mutability, layout artifact prevention ([adad36c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/adad36c))
- JSON tree detail view, search filter presets, contrast guard cadence ([898510f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/898510f))
- JSON tree clipboard copy support and contextual copy actions ([67eeec0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/67eeec0))
- Dashboard hotspot remediation with thread-local caches and constant precomputation ([75e511b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/75e511b))
- Dirty-state gated data ingestion on all TUI screens ([b9bff58](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b9bff58))
- TUI spin watchdog, sqlite auto-recovery, and highlight fix ([eff669d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eff669d))
- Lazy screen materialization, semantic db-stats diffing ([f0a09af](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f0a09af))
- Deferred background worker startup and ambient renderer cached-composite optimization ([95c4ba9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/95c4ba9))

### Database and Storage

- Durability probes, pool improvements, hardened agent/message operations ([fa9b3e9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/fa9b3e9))
- Enhanced search v3, integrity metrics, query pagination, JSONL reconstruction ([eb7b21b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eb7b21b))
- Schema migrations through canonical SQLite to prevent index corruption ([c630e7f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c630e7f))
- SQL injection fix, WAL compatibility, agent dedup, metric safety ([3eab38d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3eab38d))
- Post-migration integrity guard and strengthened quarantine test ([cbc574c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/cbc574c))
- Robust coalescer commit pipeline with structured outcomes and failure tracking ([146e54f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/146e54f))

### Installer and CLI

- SHA256 checksum verification in `install.ps1` and E2E test hardening ([8006931](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8006931))
- `--no-tui` flag, `--rollback` migration, expanded doctor checks, startup refactor ([8449aee](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8449aee))
- Service management CLI, pane identity tools, TUI scroll fixes ([7c374ff](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7c374ff))
- Eliminate stale WAL/SHM sidecar propagation during DB copy ([1ea8604](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1ea8604))
- Kafka transport enablement via `crossterm-compat` features ([cfcaa05](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/cfcaa05))

### Server

- Health signature headers, PID-aware port clearing ([9a08dad](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9a08dad))
- Attachment processing, thread ID validation, guard environment tests ([3496194](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3496194))
- Responsive breakpoint layouts and side detail panels on all screens ([6b4f66a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6b4f66a))
- HTTP liveness probe supervisor and hardened listener config ([3db82b1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3db82b1))
- Tailscale remote-access detection and display ([c602abb](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c602abb))

### Performance

- `DbWarmupState` enum for three-state DB readiness tracking ([3d2e326](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3d2e326))
- Dashboard render coalescing and lazy export snapshot refresh ([c613e9e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c613e9e))
- Resize coalescing, diff strategy, and contrast guard optimizations ([a167585](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a167585))

---

## [v0.2.1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.1) — 2026-03-03 **[Release]**

27 commits since v0.2.0 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.2.0...v0.2.1)

Focused on `am doctor fix`, TUI V2 testing, installer UX, and performance improvements.

### am doctor fix

- Automatic remediation for 6 fixable checks via `am doctor fix` subcommand ([e9a7dbe](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e9a7dbe0e5bfa08be518419a6080af9d8f5deea3))
- Bug fixes, robustness hardening, and performance improvements across core/db/server/tools ([acd475f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/acd475f))

### Installer

- `--dry-run` preview mode and piped install confirmation ([7e2f875](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7e2f875))
- Incident regression gates, robust alias displacement, E2E test hardening ([29e48dd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/29e48dd))

### TUI

- Batch `mark_unread` + 21 batch selection tests ([53a5051](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/53a5051))
- 31 V2 TUI tests across 4 modules ([30c9d43](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/30c9d43))
- Theme snapshot tests with 16ms budget enforcement ([81adf8f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/81adf8f))
- Eliminate double housekeeping tick, persist contrast-guard cache, fix search hot-loop ([18489a5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/18489a5))
- Reservation expiry-driven refresh ([7777e6d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7777e6d))

### Performance

- Static `LazyLock` regexes, `getrandom` for agent names, coalescer `worker_count` ([c821a4f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c821a4f))
- Persistent caches for cleanup prober, embedding queue drain, retry scheduling ([5eba4d5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5eba4d5))

### Testing

- Truth oracle, incident capture, and migration test infrastructure ([9981998](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9981998))
- Screen diagnostics, truth assertions, auth improvements ([afd43bd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/afd43bd))
- Scope-aware caching, FrankenSQLite compat, and correctness fixes ([bc1c340](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bc1c340))

### Security

- Replace exposed bearer token in `factory.mcp.json` ([18d50e0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/18d50e0))

---

## [v0.2.0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.2.0) — 2026-03-02 **[Release]**

325 commits since v0.1.0 | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/v0.1.0...v0.2.0)

Massive release touching every subsystem. Introduces Search V3 (two-tier Tantivy + lexical bridge architecture), the 15-screen TUI operations console, a human-readable web dashboard, write-behind queue for extreme load resilience, RBAC/JWT enforcement, console split-mode with command palette, and comprehensive E2E/conformance testing.

### Search V3 Architecture

Complete search rewrite from SQL-based FTS5 to a two-tier Tantivy + lexical bridge architecture:

- Decomposed monolithic search into focused modules with two-tier architecture ([43ec691](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/43ec691))
- Incremental Tantivy backfill with watermark-based skip ([bf7a6c2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bf7a6c2))
- Scope-aware cache discriminator to prevent cross-scope query collisions ([d376b82](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d376b82))
- CLI and robot search routed through Search V3 service ([c758017](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c758017))
- All TUI screens migrated from SQL planner to unified search service ([c94f5cd](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c94f5cd))
- Removed SQL LIKE fallback entirely ([9429825](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9429825))
- Two-tier search observability metrics and quality health reporting ([72f7328](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/72f7328), [8962bbf](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8962bbf))

### TUI Operations Console

Full-screen interactive TUI with multi-screen operations cockpit:

- 15-screen TUI: dashboard, messages, threads, agents, contacts, reservations, search, timeline, metrics, health, analytics, attachments, archive browser, and more ([7278617](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7278617), [10083df](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/10083df))
- Server-side compose dispatch via sync SQLite ([3c3e135](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3c3e135))
- Compose panel with validated send dispatch ([caf494e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/caf494e), [43c2bec](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/43c2bec))
- Mouse drag-and-drop message rethreading across screens ([b04ff78](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/b04ff78))
- Vim-style visual multi-selection with batch actions ([5e1209c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/5e1209c))
- Interactive widget inspector overlay for debugging ([76afea9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/76afea9))
- Theme integration mapping ftui palettes to TUI styles ([e22c250](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e22c250))

### Console Split-Mode

- Alt-screen split layout wired into server ([dbf52f1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/dbf52f1))
- Command palette with 25 actions and dispatch wiring ([d601d55](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d601d55))
- ConsoleCaps detection, banner, help overlay, OSC-8 hyperlink support ([1eda13e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1eda13e), [47b6fcc](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/47b6fcc))
- Event timestamps, kind filter, and detail enhancements ([6b364da](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6b364da))

### Web Dashboard

- Human-readable UI dashboard with archive browser and mail views ([342b821](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/342b821))
- RBAC/JWT enforcement, tool instrumentation, mail UI pagination ([86dd07d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/86dd07d))
- Retention engine, health endpoints, tool metrics, mail UI module ([2eb5a8f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2eb5a8f))

### Database and Storage

- v13 poller indexes, `busy_timeout` pragma, lock-retry migration engine ([8322891](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/8322891))
- v3 migration for TEXT timestamps ([50977c6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/50977c6))
- Write-behind queue for extreme load resilience ([da5e317](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/da5e317))
- Async commit coalescer for storage pipeline ([da5e317](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/da5e317))
- Expand query layer with retention, tracking, schema improvements ([c281fd5](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c281fd5))
- Retry layer, expanded error taxonomy, hardened connection pool ([a8d8101](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a8d8101))
- Three-way JOIN replaced with two-phase sampling in consistency probe ([df6e0c7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/df6e0c7))
- Drop legacy Python FTS triggers on migration to prevent constraint failures ([880a0a9](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/880a0a9))
- S3-FIFO frequency count preservation on main queue promotion ([3d393dc](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3d393dc))

### Performance

- Deferred backfill, integrity cache, persistent poller connections ([24b5636](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/24b5636))
- Startup latency optimization with redundant probe skip and minimal pool allocation ([27cd3fe](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/27cd3fe))
- Suppress noisy fsqlite tracing, minimize worker pool allocations ([44ecfc3](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/44ecfc3))
- Two-tier search index optimized with direct chunk iteration and destructuring moves ([09c2d6d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/09c2d6d))

### Security

- TOCTOU race fix in env file creation ([bba526a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bba526a))
- Enforce 0600 permissions on env files containing bearer tokens ([2acd47d](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/2acd47d))
- Path traversal prevention in agent detection module ([a827c2e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a827c2e))

### Installer

- Uninstall mode, MCP config management, Windows installer ([77b4215](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/77b4215))
- Setup self-heal fingerprint cache and preflight optimization ([3d9c9f0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/3d9c9f0))
- Fresh install surface validation suite ([84bc664](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/84bc664))

### CLI and Tools

- ~15 CLI commands implemented, replacing `NotImplemented` stubs ([935b183](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/935b183))
- CLI overhaul with rich output and expanded conformance test runner ([9953f94](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/9953f94))
- Major CLI expansion with output module, new commands, and 123+ tests ([440d358](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/440d358))
- Guard rewrite with rename and ignorecase support ([c4c742a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c4c742a))
- Glob-to-regex rewrite with `[]`, `{}` syntax support ([894ebb1](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/894ebb1))
- LLM stub mode, identity resource, tool metrics reset ([a748623](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/a748623))
- TOON output format with comprehensive tests ([285036b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/285036b), [bc0ec45](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/bc0ec45))
- am runner + MCP base-path alias ([33ab58a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/33ab58a))
- Pre-TUI startup banner, reservation validation, port migration to 8899 ([ef15f00](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ef15f00))

### Share/Export Pipeline

- Self-contained HTML viewer and improved bundle finalization ([eab8cb2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eab8cb2))
- Deterministic ZIP output, stricter crypto validation ([852fa13](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/852fa13))
- Chunked export params and share pipeline benchmarks ([73d814a](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/73d814a))

### Testing

- 54 input validation + serde tests for tool modules ([6d57e63](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6d57e63))
- E2E share/export test suite and CLI integration tests ([1c333b2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1c333b2))
- CLI stability test suite, stdio transport verification ([16df695](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/16df695), [099780f](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/099780f))
- Addressed GitHub issues #8-#18 across multiple subsystems ([d3ec890](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/d3ec890))

---

## [v0.1.0](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.1.0) — 2026-02-24 **[Release -- Initial]**

802 commits | [Compare](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/compare/213eac7750fa368ca2b39fa72e455034158023ff...v0.1.0)

Initial public release of the Rust port of [mcp_agent_mail](https://github.com/Dicklesworthstone/mcp_agent_mail). Full feature parity with the Python reference implementation plus substantial performance improvements. Development began on 2026-02-05 with the [initial commit](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/213eac7750fa368ca2b39fa72e455034158023ff).

### MCP Server

- **34 MCP tools** across 9 clusters: messaging, reservations, search, macros, build slots, identity, resources, contacts, and products
- **23+ MCP resources** with conformance-tested JSON output
- **Dual-mode interface**: MCP server (`mcp-agent-mail` binary, stdio/HTTP transport) and operator CLI (`am` binary) share the same tool implementations but enforce strict surface separation
- Tool filtering profiles and config-aware builder ([040298e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/040298e))

### Storage Layer

- **Git-backed archive** for human-auditable message history, reservations, and agent profiles ([c05bb3b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c05bb3b), [7ba9fe6](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/7ba9fe6))
- Attachment pipeline with automatic WebP conversion ([eb5bb09](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/eb5bb09))
- Advisory file locks and commit queue batching ([ec3bd47](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/ec3bd47))
- **SQLite** with WAL, connection pooling, FTS5 full-text search
- Write-behind cache with async commit coalescer

### Coordination

- **Advisory file reservations**: exclusive or shared leases on file globs with TTL
- **Pre-commit guard** for file reservation enforcement with conflict detection ([09aa77e](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/09aa77e))
- Force-release with multi-signal heuristics ([f1ccdce](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/f1ccdce))
- Query tracking and instrumentation module ([6526d80](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/6526d80))

### Share/Export Pipeline

- Full share/export pipeline with snapshot, scope, scrub, finalize, bundle, and optional encryption ([be68db2](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/be68db2))
- Deterministic ZIP output with crypto validation

### CLI

- Interactive console with split-mode layout
- ~15 operator commands for server management, diagnostics, and agent operations
- TOON output format with deterministic stub encoders ([285036b](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/285036b))

### Testing and Quality

- Conformance test suite against Python reference fixtures ([801c340](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/801c340))
- E2E test harness with guard test suite ([c4471d8](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/c4471d8))
- Benchmarks with baseline budgets and golden outputs ([891c47c](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/891c47c))

### Distribution

- Multi-platform binaries: Linux x86_64, macOS arm64, Windows x86_64 ([1c569d7](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/1c569d7b1a3f51e48c0f0d4fe97a8846a118c7a3))
- curl-bash installer with platform auto-detection and Codex CLI auto-configuration
- `mcp-agent-mail` (MCP server) and `am` (operator CLI) shipped as separate binaries
