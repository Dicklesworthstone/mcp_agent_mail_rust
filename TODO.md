# MCP Agent Mail Rust Port — TODO (Granular)

## Shared stdio client initialization (`br-1w2z0`, scoped contribution)

This shell-client change started on main `615f4b2f` and integrates `6eb6264d`.
Its real transport
control uses a privately extracted, signature-verified v0.3.41 server.
It does not qualify current Rust source or close the original direct-suite
task. EmeraldEagle's ownership and shared-root path-safety work are preserved.
The separate recovery candidate is published at `a025434e`; its native and
current compiler qualification remains blocked by ordinary RCH admission.

- [x] Read the original task and shared client's lifecycle/EOF behavior.
- [x] Verify the release checksum manifest with the pinned Minisign key,
  verify the Linux archive hash, and retain both private binary hashes.
- [x] Execute a real initialize/tools-list baseline without building Rust or
  replacing installed executables; retain isolated storage and all artifacts.
- [x] Demonstrate seven malformed initialize results accepted by the old
  client, including NULL and missing or mistyped required metadata.
- [x] Reject malformed required metadata before notifications/initialized or
  queued tools, preserving valid extensions and JSON-RPC initialization errors.
- [x] Add ten peer controls with exact success/failure exits, diagnostics and
  observed request order. Negative peer fixtures are not live-server proof.
- [x] Pass all 13 helper self-tests, including those ten initialization cases.
- [x] Exercise ordered and pipelined sessions against the signed real server:
  20 correlated replies, coordination tools, resource reads and an expected
  post-initialize unknown-tool error.
- [x] Use the returned mail ID after automatic contact mail; verify the intended
  message body and unchanged ACK through separate server process restarts.
- [x] Confirm the delivery receipt and committed canonical/inbox/outbox Git
  blobs. Read the committed tree directly; lock-free publication leaves the
  shared Git index empty.
- [x] Preserve original client deadlines and raw protocol-negative clients;
  pass Bash syntax and whitespace checks for the two changed source files.
- [x] Record scoped evidence on the existing Bead, retain its original
  acceptance/status/owner, and hand off the exact client source and limits.
- [x] Prepare the reviewed client fix, regression controls and scoped Bead
  evidence for publication on `mossytern/stdio-initialize-validation-20261124`.
- [x] Publish the client fix as `ca78ce6f` and draft PR #409. Preserve both
  stdio and recovery TODO sections when integrating main's concurrent update.
- [ ] Complete the inherited direct-client inventory and current-source suite
  qualification under the original task; do not count this contribution as
  completion of peer path-safety work or Rust recovery qualification.

Evidence and retained replay state:
`/data/tmp/am-mossytern-stdio-release-g0zrolbg/`. The baseline and final helper
logs retain all ten cases. `verified-binaries.json` binds release identity;
`real-client-verification.json` binds the client/self-test hashes, actual mail
ID, correlated replies, restarted receipt and committed archive copies.

## Fresh review of concurrent recovery publication (`br-u9bhg`)

This checkout integrates main `615f4b2f`, including incremental recovery
seals, profile durability repair and Git contention changes. PR #408 was
merged as `13ef2cea`; its older static receipts do not qualify this tree.
The concurrent probes are still uncompiled and have executed zero assertions.

The original `c4e665f7` probe was invalid: replacing ACK 444 with ACK 999
conflicted with the intentional first-ACK/earliest-timestamp contract. Its
source and refusal logs remain preserved. The corrected scenario publishes
a first read at 444, then attempts a first ACK at 999 with the production
`COALESCE` rule and an explicit SQL precondition that ACK is still NULL.

- [x] Reread every published Rust hunk, the storage flush/clone regression,
  durable Git debt, state-lock ordering and incomplete-publication repair.
- [x] Refresh Beads/BV, git and peer activity; keep the original qualification
  task in progress rather than taking a second unfinished soak task.
- [x] Verify live disk reservations before the normal native request.
- [x] Preserve the first DB request's terminal queue-timeout refusal: strict
  RCH on `vmi1149989`, exit 103, no Cargo execution or native assertions.
- [x] Preserve the new probes' unavailable-worker refusal (exit 103) and
  subsequent selected-worker disk-admission refusal (exit 120, RCH-E330).
  Both happened before Cargo; native assertions remain zero.
- [x] Isolate a new baseline probe from the published checkout's source freeze.
- [x] Reuse the real database setup for file-backed canonical and runtime
  connections, with concurrent autocommit enabled on the runtime peer.
- [x] Correct the invalid timestamp-overwrite model without changing the
  production first-ACK or earliest-timestamp behavior.
- [x] Add the interrupted first-read control, requiring an actual read-444,
  ACK-NULL artifact and incomplete witness before the competing commit.
- [x] Require ACK NULL immediately before the peer's real COALESCE update;
  retain actual SQL ACK-999 and recovered ACK-999 assertions.
- [x] Execute the supporting system-SQLite WAL schedule: seven SQL assertions
  confirm a first ACK can commit while an older read snapshot remains open,
  and a later COALESCE update preserves it. This is not Rust runtime evidence.
- [x] Retain the positive retry control when writer admission excludes the peer.
- [x] Integrate current main and read the recovery seal/publication changes;
  preserve all older source and failed receipts.
- [x] Repair observed workspace formatting failures in five upstream storage
  paths by applying the reported whitespace changes manually.
- [x] Pass the current workspace formatting check.
- [x] Complete normal-rule UBS across the 41-path current scope: 11 fresh
  scans and 30 byte-identical report reuses, with all 52 reviewed critical,
  56,497 warning and 20,651 informational source findings retained.
- [x] Attempt the locked current-tree all-targets check through strict RCH:
  hz4 refused at disk admission (E330/exit 120), before Cargo, growth zero.
- [ ] Compile and execute both new baseline probes through ordinary strict
  RCH admission; distinguish a target failure from an invalid control.
- [ ] If the target fails, implement the smallest production repair supported
  by that failure, preserving cancellation and caller-owned transactions.
- [ ] Execute both probes again, retain the original failure, and require
  successful peer commit/publication after the repaired operation.
- [ ] Pass all three required locked workspace compiler/Clippy graphs on the
  exact final source and complete its changed-file scanner review.
- [ ] Execute the original DB, storage, tools and CLI acceptance lanes; keep
  their nonzero counts separate from compile, scanner and admission results.
- [x] Record the invalid-probe correction and current scope in the original
  bead, flush only intended graph records, confirm zero cycles, and hand off
  the exact source and limitations. Comment 4793 supersedes 4792; the prior
  probe and mail 310 are preserved as historical, unexecuted evidence.

The superseded probe's source SHA-256 was
`617c462a2874e101ee0934662997a3838657f944ba871f5748d656216d906e80`.
Historical `concurrent-ack-v2-*` evidence remains under
`/data/tmp/am-mossytern-20261123/`. It supplies no runtime or corrected-source
qualification. Current failures and results use the `current-main-*` prefix.
No test limit, disk reservation, worker drain or peer job was changed.
Current probe source SHA-256:
`f8e46b6dd405627d5f4e443f8d19ef7afe6ac49993ece89942047ac712c896ff`.
The complete static report is `current-main-v1-ubs-final-report.json`
(SHA-256 `39c368878852ad8323437e139ca68de097db525f7d22be6b61f481e9087e2da2`).
The initial aggregate failed to normalize absolute scanner paths; its original
bytes remain explicitly marked invalid, and the corrected aggregate asserts
the prior finding counts for all reused files. No finding was suppressed.

## Published recovery qualification history (`br-u9bhg`, MossyTern)

Current base: `c5672259`. Its durable Git outbox is upstream work, reviewed
here without claiming authorship or native qualification. The superseded
inventory candidate and its complete source/Beads history remain intact at
`/data/projects/am-mossytern-b4-v5-preserved-20261124`; earlier work remains at
`/data/projects/mcp_agent_mail_rust-mossytern-20261123`.

- [x] Read the new upstream outbox, publication ordering, input bounds,
  compaction, runtime-file exclusion, and real cross-process controls.
- [x] Incorporate and read the disjoint upstream readiness refresh and its
  real-supervisor regression; keep its native proof separate and unclaimed.
- [x] Preserve all failed RCH/scanner receipts and the older candidate;
  recover the timed-out wrapper with terminal ownership acknowledged.
- [x] Carry only observed compiler repairs: separate the credential lookup's
  doc summary, use equivalent lifecycle error patterns, and retain the
  conformance nonempty credential assertion with strict-Clippy syntax.
- [x] Apply the equivalent byte-string newline comparison in the upstream
  outbox and retain its nonempty-debt test with a concrete byte-slice assertion.
- [x] Preserve the outbox-empty and zero-hint requirements with concrete
  comparisons in the DB and exited-producer storage regressions.
- [x] Strengthen the existing production flush/clone regression by draining
  old hints, discarding later receipt/contact/cursor hints, restoring the old
  batch, and requiring the cloned completion witness. Keep all prior checks.
- [x] Correct the README FAQ native-tool count to 9 (47 total minus 38
  compatibility tools). No inventory or compatibility gate was weakened.
- [x] Pass formatting and whitespace on this current candidate.
- [x] Complete normal-rule UBS across the same 36 Rust paths, refreshing
  changed files and reusing completed reports only for identical source.
- [x] Pass locked workspace all-targets check through strict RCH.
- [x] Pass locked workspace production lib/bins check through strict RCH.
- [x] Pass strict locked workspace all-targets Clippy through RCH.
- [x] Verify identical maps of 3,946 files across the three final receipts,
  with zero local hash, length or executable-mode mismatches.
- [x] Attempt the original DB native suite through strict RCH with unchanged
  limits: admission refused before Cargo (E330, exit 120), zero assertions.
- [ ] Execute DB outbox/reconstruction/cursor/sync regressions with nonzero
  counts, including malformed inputs, interrupted publication and retries.
- [ ] Execute storage Git/flush/clone, exited-producer, envelope and backlog
  controls; execute tools consuming-read/admission/committed-ACK replay and
  CLI metadata preservation lanes from the original acceptance.
- [ ] Resolve the retained concurrent-commit publication uncertainty through
  actual runtime evidence; the earlier writer-admission experiment is unrun.
- [x] Verify final source identity, retain every failure, update only this
  bead, flush its scoped JSONL and confirm zero dependency cycles.
- [x] Commit and push the reviewable work as draft PR #408, with native
  assertions explicitly zero and `br-u9bhg` still in progress.
- [ ] Complete original native acceptance and current integration qualification
  after PR #408's merge; publication does not establish release readiness.
- [x] Hand off current results in Agent Mail message 303 and release all
  nine source/TODO reservations (2026-11-24 01:46:06 UTC).

Draft: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/pull/408
Code commit: `44787e6b`. Latest target main was `bad3b904`; GitHub reports
the draft merges without conflicts. The checks above qualify the named
`c5672259` candidate, and do not claim the moving target's runtime result.

The final `c5672259` candidate passed all three static graphs with identical
maps of 3,946 files. Evidence is retained in
`/data/tmp/am-mossytern-20261123/mainc567-v1-three-gates-verified.json` and
`mainc567-v1-ubs-final-report.json`. The qualified pre-handoff source patch
SHA-256 is `1b5c383cebce69fee610b1da3998e793388e7e0195d049aa003e35e52f33b99e`.
Native assertions remain zero: disk admission and a draining worker refused
the requests. Compiler calls use a 3600-second outer envelope after a cold
1800-second timeout; test limits, disk budgets and assertions are unchanged.

## Current operational reliability improvements

- [x] Bound project identity cache retention by freshness, entry count, aggregate
  string bytes, and per-identity bytes; return oversized discovery results
  unchanged without retaining them (`br-xai5l`).
- [x] Prepare and validate reconciliation attachments before global archive
  publication admission, with cancellation checkpoints and final under-lock
  disk/Git/ancestor authority rechecks (`br-5a5s0`).
- [x] Revalidate queued ATC releases against the current engine, population,
  policy and activity; use a bounded holder snapshot and guarded database
  mutation, with atomic grant/renewal activity and no voluntary-release intent
  fallback (`br-qo8j5`).
- [x] Carry request-token and request-scope liveness through database checkout,
  writer admission and pre-commit checks for automatic releases; roll back
  observed cancellation before commit admission (`br-qo8j5`).
- [x] Run native identity regressions: 150 passed, 0 failed, 1 ignored,
  including all five new cache cases (`br-xai5l`).
- [x] Pass the core production library check and strict core all-targets Clippy.
- [ ] Complete archive/ATC native regressions and final workspace compiler/lint
  validation. On this executor, both workspace check graphs (all targets and production
  libraries/binaries) and strict workspace Clippy were killed with SIGKILL
  while compiling the pinned Asupersync dependency before reaching the
  application changes, including a single-job, no-debug, dependency opt-level
  zero retry. Broader daemon memory attribution and archive soak acceptance
  remain open.
- [ ] Prevent concurrent deliveries from losing notification wakeups during
  inbox reads, including cross-process debounce and filtered/peek behavior
  (`br-dksvp`).
- [x] Bind ATC write mode and kill-switch authority to each server lifetime;
  prevent delayed observations or controls from reusing another mailbox policy
  and fail closed when the current marker authority cannot be inspected.
- [x] Make archive flushes honor FIFO persistence barriers and report unresolved
  write failures; retain failure status across unrelated successful writes,
  shutdown, and restart until the existing repair ledger verifies recovery.
- [x] Reject invalid or unresolved query filters before search routing and caching;
  preserve explicit facet precedence and propagate project lookup failures and
  cancellation without broadening the search.
- [x] Publish archive epoch and index-sync control markers through private,
  exclusively created staging files; preserve foreign staging entries and reject
  symlinked control paths and nonregular destinations.
- [x] Make glob reservation discovery lazy and bounded without hashing tracked
  file contents; honor Git ignore rules and preserve unknown activity for partial
  scans, nested repositories, and submodules before force-release decisions.
- [x] Keep descriptor-floor monitoring safe during declining counts, sampler
  restarts, and missed windows; use observed minima timestamps for growth and
  expose a conditional soft-limit exhaustion estimate in health and logs.
- [ ] Complete the broader `br-kp1in.29` operation-normalized resource-growth,
  statistical calibration, shared soak predicates, and release-overhead acceptance.

## 0. Coordination / Meta
- [x] Read `AGENTS.md` and `README.md` (baseline orientation).
- [x] Register with MCP Agent Mail and announce to active agents.
- [x] Check inbox from other agents at least once per session.
- [x] Reserve files before edits (Agent Mail leases) and release on completion.
- [x] Keep `FEATURE_PARITY.md` aligned with completed work.
- [x] Keep `TODO.md` updated after each batch of changes.
- [x] Confirm MCP Agent Mail server stability (health check path, lock handling).

## 1. Workspace & Crate Scaffolding
- [x] Workspace `Cargo.toml` with required path deps.
- [x] `rust-toolchain.toml` with nightly channel.
- [x] Crate directories for core/db/storage/server/cli/guard/share/conformance.
- [x] Scaffold minimal `Cargo.toml` for each crate.
- [x] Placeholder `lib.rs`/`main.rs` for each crate.
- [x] README for Rust port (overview, usage, dev commands).

## 2. Conformance Harness (Skeleton → Full)
- [x] `crates/mcp-agent-mail-conformance/tests/conformance/README.md` with schema.
- [x] Fixtures placeholder JSON + loader.
- [x] Python fixture generator stub + full runner.
- [x] Rust test to load fixture schema.
- [x] Python reference runner: start legacy server + capture tool outputs.
- [x] Python reference runner: capture resources + deterministic ordering.
- [x] Canonical fixture schema for ok/err shapes.
- [x] Rust conformance runner: tools + resources + diff reporting.
- [x] Tool-filter fixtures (profiles + include/exclude).
- [x] Archive artifact assertions (profiles, messages, inbox/outbox, reservations).
- [x] Notification signal conformance (to/cc signals, bcc excluded, fetch_inbox clears).
- [x] Conformance CLI helper to regenerate fixtures (Rust wrapper or script).
- [x] CI step for conformance tests (ensure fixtures are current).
- [x] Add missing error-case fixtures (invalid params, unknown project/agent, malformed inputs).
- [x] Add resource query-param fixtures (format/filter params for static resources).

## 3. DB Layer (sqlmodel_rust) — Models & Schema
- [x] SQLModel structs for all tables (`src/models.rs`).
- [x] JSON attachments field in message schema.
- [x] Timestamp conversion utilities (`timestamps.rs`).
- [x] Error types (`error.rs`).
- [x] Explicit indexes + unique constraints matching legacy.
- [x] FTS triggers installed and validated.
- [x] Schema migration story using `sqlmodel_schema::MigrationRunner`.
- [x] WAL PRAGMA setup SQL (`schema.rs`).
- [x] Pool config defaults (`pool.rs`).
- [x] Pool wired to `sqlmodel_pool` + `sqlmodel_sqlite`.
- [x] Query tracking + slow query logging (per-tool instrumentation).
- [x] Verify tracked connection coverage for any direct `query_sync` tool paths.

## 4. Server Layer (fastmcp_rust)
- [x] All tools registered with correct names and clusters.
- [x] All resources registered with correct URI templates.
- [x] Tool filtering profiles wired (full/core/minimal/messaging/custom).
- [x] Identity tools: ensure_project/register_agent/create_agent_identity/whois.
- [x] Messaging tools: send_message/reply_message/fetch_inbox/mark_message_read/acknowledge_message.
- [x] Search tools: search_messages/summarize_thread.
- [x] File reservation tools: file_reservation_paths/release/renew/force_release.
- [x] Contact tools: request_contact/respond_contact/list_contacts/set_contact_policy.
- [x] Product tools: ensure_product/products_link/search_messages_product/fetch_inbox_product/summarize_thread_product.
- [x] Build slots: acquire/renew/release.
- [x] Macros: start_session/prepare_thread/file_reservation_cycle/contact_handshake.
- [x] TOON output handling for tool + resource responses.
- [x] Instrumentation wrapper for per-tool query stats logging.
- [x] Validate resource query matching behavior against fastmcp router (edge cases).
- [x] Implement query parsing for static resources (`resource://projects?format=...`).

## 5. Storage Layer (Git Archive)
- [x] Archive root creation + `.gitattributes`.
- [x] Per-project locks (`.archive.lock`) + commit locks.
- [x] Commit queue batching.
- [x] Message write pipeline (canonical + inbox/outbox + commit).
- [x] File reservation artifacts (digest + id files).
- [x] Attachment pipeline (WebP conversion, originals, manifests, audit logs).
- [x] Notification signals (debounced files).
- [x] Read helpers (message by id, thread digest, commit history).
- [x] Storage wired into identity, messaging, reservation, resources.

## 6. Guard (Pre-commit/Pre-push)
- [x] Guard install/uninstall (chain-runner hook).
- [x] Pre-commit check (staged paths, rename handling).
- [x] Pre-push check (commit diff scanning).
- [x] Path matching (pathspec + fnmatch fallback).
- [x] Advisory mode + bypass env var.
- [x] Rust-native conflict detection (guard_check, guard_check_full).
- [x] Guard status command.

## 7. Share / Export
- [x] SQLite snapshot (WAL checkpoint + backup).
- [x] Scrub presets (standard/strict/archive).
- [x] FTS + materialized views for static viewer.
- [x] Attachment bundling (inline/detach thresholds).
- [x] Chunking for large DB.
- [x] Manifest + signing + encryption (age/Ed25519).
- [x] Preview server + verify/decrypt.

## 8. CLI
- [x] `serve-http`, `serve-stdio`.
- [x] `lint`, `typecheck`.
- [x] `guard` subcommands (install/uninstall/status/check).
- [x] `acks` subcommands (pending/remind/overdue).
- [x] `file_reservations` subcommands (list/active/soon).
- [x] `config` subcommands (set-port/show-port).
- [x] `doctor check` (--verbose, --json).
- [x] `projects` subcommands (mark-identity/discovery-init/adopt).
- [x] `list-projects` (--json, --include-agents).
- [x] `list-acks`.
- [x] `mail status`.
- [x] `migrate`.
- [x] `clear-and-reset-everything`.
- [x] `am-run` and `amctl env`.
- [x] `share` subcommands (export/update/preview/verify/decrypt/wizard).
- [x] `archive` subcommands (save/list/restore).
- [x] `doctor` subcommands (repair/backups/restore).
- [x] `products` subcommands (ensure/link/status/search/inbox/summarize-thread).
- [x] `docs insert-blurbs`.

## 9. Benchmarks (Perf + Conformance)
- [x] Bench suite reusing conformance fixtures.
- [x] Tool latency baseline (8 representative tools).
- [x] Initial baseline recorded (most tools sub-100µs).
- [x] Add archive write throughput benchmarks (send_message + commit queue).
- [x] Capture hyperfine baselines for CLI commands (serve + send_message + fetch_inbox).
- [x] Run `cargo flamegraph` for hot paths (search + send_message + storage writes).
- [x] Record golden outputs + checksums for optimization validation.
- [x] Maintain perf regression notes in `FEATURE_PARITY.md` or dedicated doc.

## 10. Dependency Integration (Local /dp Crates)
- [x] Confirm zero direct `tokio` usage in code paths (only via optional deps).
- [x] Use `sqlmodel_rust` for SQLite access (no direct `rusqlite`).
- [x] Use `asupersync` for HTTP + async runtime.
- [x] Use `frankentui` (`ftui`) for CLI rendering.
- [x] Integrate `beads_rust` for issue/task awareness in CLI or server.
- [x] Integrate `coding_agent_session_search` for agent detection (or port logic to avoid tokio).
- [x] Evaluate fastmcp resource query param handling; add adapters if needed (validated by router-level conformance tests; no adapter required).

## 11. Testing & QA
- [x] Unit tests for tool validation rules (input constraints, error messages).
- [x] Integration tests for DB + storage pipelines (archive + SQLite consistency).
- [x] E2E tests for MCP tool calls (stdio + HTTP).
- [x] Guard tests for conflict detection (pathspec + rename + advisory).
- [x] Run full toolchain: `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test`.

## 12. Publishing & Release (Crates.io + CI)
- [x] Confirm Cargo metadata (license/readme/repository) for publishable crates.
- [x] Add crates.io publish workflow gated by tags (`CARGO_REGISTRY_TOKEN`).
- [x] Document publish order for workspace crates (`cargo publish -p ...`).

### Publish Order (dependency-respecting)

```bash
# Phase 1: Foundation (no internal deps)
cargo publish -p mcp-agent-mail-core

# Phase 2: Database + storage layers
cargo publish -p mcp-agent-mail-db
cargo publish -p mcp-agent-mail-storage

# Phase 3: Utilities
cargo publish -p mcp-agent-mail-guard
cargo publish -p mcp-agent-mail-share

# Phase 4: High-level APIs
cargo publish -p mcp-agent-mail-tools
cargo publish -p mcp-agent-mail-server
cargo publish -p mcp-agent-mail-cli

# Phase 5: Binaries
cargo publish -p mcp-agent-mail

# Not published: mcp-agent-mail-conformance (publish = false)
```

**Note**: All external deps (fastmcp, sqlmodel, asupersync, ftui) are path dependencies
and must be published to crates.io first or replaced with registry versions before
workspace crates can be published.
