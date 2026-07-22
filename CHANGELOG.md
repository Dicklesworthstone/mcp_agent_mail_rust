# Changelog

All notable changes to [MCP Agent Mail (Rust)](https://github.com/Dicklesworthstone/mcp_agent_mail_rust) are documented in this file.

Versions marked **[Release]** have published [GitHub Releases](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases) with downloadable binaries. Versions marked **[Tag only]** exist as git tags but were never published as GitHub Releases.

Release sequencing now lives in [docs/RELEASE_TRAIN_PLAN.md](docs/RELEASE_TRAIN_PLAN.md), and per-release sign-off packets should start from [docs/RELEASE_READINESS_TEMPLATE.md](docs/RELEASE_READINESS_TEMPLATE.md).

---

## Unreleased

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
